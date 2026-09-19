//! Foreground Unix socket supervisor. Client lifetimes never own child PTYs.
pub mod auth;
mod ring_buffer;
pub mod sessions_catalog;
mod ticket;
mod ws;
pub mod ws_proto;
use crate::auth::{Allowlist, PrincipalId};
use crate::ring_buffer::{RingBuffer, RING_BUFFER_CAP};
use crate::ticket::TicketRegistry;
use aihub_core::*;
use aihub_pty::{PtySize, PtySpawnOptions};
use anyhow::{anyhow, bail, Result};
use rand::Rng;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, UnixListener},
    sync::{broadcast, mpsc, Mutex},
    task::JoinSet,
};

pub type Operation<T> = Pin<Box<dyn Future<Output = Result<T>> + Send>>;

/// A stop barrier could not be confirmed (F4/F5): the client sees `stop_unconfirmed`, never the
/// generic request-failed code, so merge/switch refusal is distinguishable from other failures.
#[derive(Debug, thiserror::Error)]
#[error("stop could not be confirmed: {0}")]
pub struct StopUnconfirmed(String);

fn project_identity(originating_checkout: &Path) -> String {
    originating_checkout
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| originating_checkout.display().to_string())
}

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

pub fn spawn_pty(
    harness: HarnessId,
    options: PtySpawnOptions,
    model: Option<String>,
) -> Result<Pty> {
    let handle = if let Ok(cmd) = std::env::var("AIHUB_PTY_COMMAND") {
        Arc::new(aihub_pty::spawn_command(&cmd, &[], options)?)
    } else {
        Arc::new(aihub_pty::spawn_harness(
            harness,
            options,
            model.as_deref(),
        )?)
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

/// Whether the process-group stop barrier has been confirmed for `generation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupStopState {
    /// Outgoing harness may still be running or a descendant may still be in the group.
    Pending,
    /// `stop` succeeded for this `generation`; merge/switch may finalize.
    Confirmed,
    /// `stop` failed; destructive work must retry the barrier instead of trusting `active`.
    Unconfirmed,
}

struct Session {
    summary: SessionSummary,
    base: String,
    originating_checkout: PathBuf,
    prompt: Option<String>,
    pty: Pty,
    generation: u64,
    group_stop: GroupStopState,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    hold_task: Option<tokio::task::JoinHandle<()>>,
    recommendation_id: u64,
    current_recommendation: Option<RouteOutcome>,
    /// Principal that created this session (01-transporte-e-sessao.md §4). Attach and every
    /// other session-scoped operation are vetoed for any other principal, even if they know
    /// the `SessionId` — mere possession of the handle confers no authorization.
    owner: PrincipalId,
    /// Ring buffer de retomada por offset (2 MiB, ADR §2.2/§3): persiste através de troca de
    /// harness (`switch()` nunca o substitui, só `push`a o banner do PTY entrante nele).
    ring: RingBuffer,
    /// Espelha os mesmos bytes de `ring.push` para a conexão de PTY dedicada (canal duplo,
    /// ADR contradição 1). Distinto do broadcast JSON em `state.clients`: este carrega
    /// `(stream_offset, data)` cru, consumido pelo canal binário sem overhead de Base64/JSON.
    pty_channel_tx: broadcast::Sender<(u64, Vec<u8>)>,
}

/// Pending destructive confirmation for `MergeRequest` over a network-authenticated connection
/// (ADR §2.3 nível 3). Distinct from the plain UDS "repeat to confirm" flow: here confirmation
/// is a fresh signed envelope over a one-time nonce, delivered as a second `Hello.credential`
/// on the same connection, never a bare repeat of `MergeRequest` itself.
#[derive(Clone)]
struct MergeConfirm {
    session_id: SessionId,
    strategy: MergeStrategy,
    diff: String,
    nonce: Vec<u8>,
    path: PathBuf,
    base: String,
    branch: String,
    origin: PathBuf,
}

struct HoldDispatchPlan {
    harness: HarnessId,
    hold_s: u64,
    model: Option<String>,
    lane: Option<String>,
    rec_id: u64,
}
struct Client {
    sender: mpsc::Sender<DaemonMessage>,
    attached: HashSet<SessionId>,
    // Contract has no separate preview message: repeat a matching request to confirm.
    merge: Option<(SessionId, MergeStrategy, String)>,
    /// Identity established at handshake (ADR §2.3): `PrincipalId::local()` for every Unix
    /// socket connection, a key fingerprint for a network connection that proved possession.
    principal: PrincipalId,
    /// Set while a network principal's destructive `MergeRequest` awaits its signed envelope
    /// (see `MergeConfirm`). Always `None` for local connections.
    merge_confirm: Option<MergeConfirm>,
}
#[derive(Default)]
struct State {
    sessions: Vec<Session>,
    clients: HashMap<u64, Client>,
    next_id: u64,
    /// `channel_ticket`s emitidos e ainda não redimidos (ADR contradição 1, Opção B).
    tickets: TicketRegistry,
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

type Spawner = dyn Fn(HarnessId, PtySpawnOptions, Option<String>) -> Result<Pty> + Send + Sync;
type Probe =
    dyn Fn() -> Pin<Box<dyn Future<Output = Vec<QuotaSnapshot>> + Send + 'static>> + Send + Sync;
type Classifier = dyn Fn(&str) -> Pin<Box<dyn Future<Output = aihub_router::Classification> + Send + 'static>>
    + Send
    + Sync;
type Router = dyn Fn(
        TaskTier,
        TaskSize,
        &[QuotaSnapshot],
        u64,
        &aihub_router::ModelCatalog,
    ) -> Result<RouteOutcome>
    + Send
    + Sync;
type CatalogPath = dyn Fn(HarnessId) -> Option<PathBuf> + Send + Sync;
type Drain =
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'static>> + Send + Sync;
type MemoryRecorder = dyn Fn(
        &SessionId,
        HarnessId,
        HarnessId,
        &aihub_memory::BriefPair,
        &str,
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
    catalog: Arc<Mutex<aihub_router::ModelCatalog>>,
    catalog_paths: Arc<CatalogPath>,
    drain: Arc<Drain>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Owner-approved public keys (ADR §2.3). Default empty: loaded once at construction via
    /// `Allowlist::load()`, never re-read per connection.
    allowlist: Arc<Allowlist>,
    /// Durable audit log path (`~/.local/share/aihub/log/audit.jsonl` by default, ADR §2.3 §6).
    audit_path: PathBuf,
    /// Path to `sessions.json` (ADR contradição 3, Opção B). `None` by default (`Daemon::new`)
    /// so tests never touch a real `~/.local/share/aihub` — only `with_sessions_catalog_path`
    /// (and `main.rs`, which always sets it) enables persistence/reconciliation.
    catalog_path: Option<PathBuf>,
}

impl Daemon {
    pub fn new<P, F, S>(probe: P, spawner: S) -> Self
    where
        P: Fn() -> F + Send + Sync + 'static,
        F: Future<Output = Vec<QuotaSnapshot>> + Send + 'static,
        S: Fn(HarnessId, PtySpawnOptions, Option<String>) -> Result<Pty> + Send + Sync + 'static,
    {
        let clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
        let clock_router = clock.clone();
        Self {
            state: Arc::new(Mutex::new(State::default())),
            cache: Arc::new(Mutex::new(vec![])),
            clock,
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
            router: Arc::new(move |tier, size, snapshots, horizon, catalog| {
                aihub_router::route_outcome_at(
                    tier,
                    size,
                    snapshots,
                    horizon,
                    catalog,
                    clock_router(),
                )
                .map_err(Into::into)
            }),
            memory_recorder: Arc::new(|session_id, from, to, brief, project| {
                let id = session_id.clone();
                let b = brief.clone();
                let project = project.to_string();
                let id_call = id.clone();
                let b_call = b.clone();
                let project_call = project.clone();
                Box::pin(async move {
                    let join = tokio::task::spawn(async move {
                        aihub_memory::record_handoff_destination(
                            &id_call,
                            from,
                            to,
                            &b_call,
                            &project_call,
                        )
                        .await
                    })
                    .await;
                    match join {
                        Ok(Ok(dest)) => Ok(dest),
                        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
                        Err(_) => {
                            aihub_memory::record_handoff(&id, from, to, &b, &project).await?;
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
            catalog: Arc::new(Mutex::new(aihub_router::ModelCatalog::default())),
            // No default catalog executables: tests must never spawn a real `cursor-agent`/`agy`
            // by accident. main.rs opts in explicitly via `with_catalog_paths`.
            catalog_paths: Arc::new(|_| None),
            // No-op by default: tests must never poll a real ai-memory on 127.0.0.1:49374.
            // main.rs opts in explicitly via `with_drain`.
            drain: Arc::new(|| Box::pin(async { Ok(0) })),
            allowlist: Arc::new(Allowlist::load()),
            audit_path: auth::audit_log_path(),
            catalog_path: None,
        }
    }

    /// Enables `sessions.json` persistence and startup orphan reconciliation at `path`
    /// (ADR contradição 3, Opção B). Production (`main.rs`) always sets this; tests only set
    /// it when they specifically exercise the catalog, so ordinary tests never touch disk.
    pub fn with_sessions_catalog_path(mut self, path: PathBuf) -> Self {
        self.catalog_path = Some(path);
        self
    }

    /// Overrides the owner-approved public key allowlist (default: `Allowlist::load()`, empty
    /// unless the owner configured one via env/file — ADR §2.3).
    pub fn with_allowlist(mut self, allowlist: Allowlist) -> Self {
        self.allowlist = Arc::new(allowlist);
        self
    }

    /// Overrides the durable audit log path (default: `~/.local/share/aihub/log/audit.jsonl`).
    pub fn with_audit_log_path(mut self, path: PathBuf) -> Self {
        self.audit_path = path;
        self
    }

    pub fn with_catalog_paths<C>(mut self, resolver: C) -> Self
    where
        C: Fn(HarnessId) -> Option<PathBuf> + Send + Sync + 'static,
    {
        self.catalog_paths = Arc::new(resolver);
        self
    }

    pub fn with_drain<D, F>(mut self, drain: D) -> Self
    where
        D: Fn() -> F + Send + Sync + 'static,
        F: Future<Output = Result<usize>> + Send + 'static,
    {
        self.drain = Arc::new(move || Box::pin(drain()));
        self
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
        R: Fn(
                TaskTier,
                TaskSize,
                &[QuotaSnapshot],
                u64,
                &aihub_router::ModelCatalog,
            ) -> Result<RouteOutcome>
            + Send
            + Sync
            + 'static,
    {
        self.router = Arc::new(router);
        self
    }

    pub fn with_clock<C>(mut self, clock: C) -> Self
    where
        C: Fn() -> u64 + Send + Sync + 'static,
    {
        let clock = Arc::new(clock);
        self.clock = clock.clone();
        let clock_router = clock.clone();
        self.router = Arc::new(move |tier, size, snapshots, horizon, catalog| {
            aihub_router::route_outcome_at(tier, size, snapshots, horizon, catalog, clock_router())
                .map_err(Into::into)
        });
        self
    }

    pub fn with_catalog(mut self, catalog: aihub_router::ModelCatalog) -> Self {
        self.catalog = Arc::new(Mutex::new(catalog));
        self
    }

    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    pub async fn check_capacity(&self, harness: HarnessId, lane: Option<&str>) -> bool {
        let snapshots = self.cache.lock().await;
        if snapshots.is_empty() {
            return true;
        }
        for snap in snapshots.iter() {
            if snap.slot.harness == harness {
                if snap.status == QuotaStatus::Empty {
                    return false;
                }
                if let Some(target_lane) = lane {
                    if let Some(l) = snap.lanes.iter().find(|l| l.name == target_lane) {
                        if l.windows.iter().any(|w| w.used_pct >= 100.0) {
                            return false;
                        }
                    }
                }
                return true;
            }
        }
        true
    }

    pub fn with_memory_recorder<M, F>(mut self, recorder: M) -> Self
    where
        M: Fn(&SessionId, HarnessId, HarnessId, &aihub_memory::BriefPair, &str) -> F
            + Send
            + Sync
            + 'static,
        F: Future<Output = Result<aihub_memory::HandoffDestination>> + Send + 'static,
    {
        self.memory_recorder = Arc::new(move |s, f, t, b, p| Box::pin(recorder(s, f, t, b, p)));
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
        self.add_locked(&mut state, repo, wt, harness, prompt, PrincipalId::local())
    }

    /// Same as `add_session`, but bound to an explicit owner (ADR §2.3, §4) instead of the
    /// local pseudo-principal. Lets tests exercise cross-principal attach without a real
    /// network handshake for the session-creation step.
    pub async fn add_session_owned(
        &self,
        repo: PathBuf,
        wt: aihub_git::SessionWorktree,
        harness: HarnessId,
        prompt: Option<String>,
        owner: PrincipalId,
    ) -> Result<SessionId> {
        let mut state = self.state.lock().await;
        self.add_locked(&mut state, repo, wt, harness, prompt, owner)
    }

    /// Explicitly switches a session to a target harness, returning the handoff destination.
    pub async fn switch_session(
        &self,
        id: &SessionId,
        target: HarnessId,
        handoff: bool,
    ) -> Result<Option<aihub_memory::HandoffDestination>> {
        self.switch_and_deliver(id, target, handoff, None).await
    }

    /// Prepares a switch under the registry lock, then delivers the handoff (if any) after
    /// releasing it, so a hung sidecar never blocks other clients or shutdown (N1).
    async fn switch_and_deliver(
        &self,
        id: &SessionId,
        target: HarnessId,
        handoff: bool,
        model: Option<&str>,
    ) -> Result<Option<aihub_memory::HandoffDestination>> {
        let pending = {
            let mut state = self.state.lock().await;
            self.switch(&mut state, id, target, handoff, model).await?
        };
        let Some((old, brief, project)) = pending else {
            return Ok(None);
        };
        match (self.memory_recorder)(id, old, target, &brief, &project).await {
            Ok(dest) => {
                log_lifecycle(
                    "INFO",
                    "session switch",
                    &format!(
                        "session_id={} from={} to={} handoff_dest={:?}",
                        id, old, target, dest
                    ),
                );
                Ok(Some(dest))
            }
            Err(err) => {
                let spool_full = err
                    .downcast_ref::<aihub_memory::MemoryError>()
                    .is_some_and(|e| matches!(e, aihub_memory::MemoryError::SpoolFull(_)));
                log_lifecycle(
                    "ERROR",
                    "handoff_record",
                    &format!("session_id={} handoff record failed: {}", id, err),
                );
                let mut state = self.state.lock().await;
                if spool_full {
                    state.broadcast(error(
                        "handoff_spool_full",
                        "Handoff spool storage limit reached; harness switched without a durable audit record",
                    ));
                } else {
                    state.broadcast(error(
                        "handoff_record",
                        "Harness switched, but handoff metadata could not be recorded",
                    ));
                }
                Ok(None)
            }
        }
    }

    /// Stops a session through the confirmed group barrier (F4). On error the session is left
    /// visibly stopped rather than retried, and the caller must not finalize Git or launch a
    /// replacement harness.
    async fn quiesce(&self, state: &mut State, id: &SessionId) -> Result<()> {
        let s = state.session(id)?;
        if s.group_stop == GroupStopState::Confirmed {
            return Ok(());
        }
        let stop_fn = s.pty.stop.clone();
        let result = stop_fn(Duration::from_secs(5)).await;
        let s = state.session(id)?;
        match result {
            Ok(_) => {
                s.group_stop = GroupStopState::Confirmed;
                s.summary.active = false;
                for task in s.tasks.drain(..) {
                    task.abort();
                }
                log_lifecycle("INFO", "session stop", &format!("session_id={id} stopped"));
                Ok(())
            }
            Err(e) => {
                s.group_stop = GroupStopState::Unconfirmed;
                s.summary.active = false;
                log_lifecycle(
                    "ERROR",
                    "stop_unconfirmed",
                    &format!("session_id={id} stop barrier failed: {e}"),
                );
                Err(StopUnconfirmed(format!("session {id} could not be confirmed stopped")).into())
            }
        }
    }

    /// Quiesces the session and finalizes the git merge/discard (shared by the local
    /// "repeat MergeRequest to confirm" flow and the network signed-envelope flow above).
    #[allow(clippy::too_many_arguments)]
    async fn finish_merge(
        &self,
        state: &mut State,
        client: u64,
        session_id: SessionId,
        strategy: MergeStrategy,
        diff: String,
        path: PathBuf,
        base: String,
        branch: String,
        origin: PathBuf,
    ) -> Result<()> {
        // F4: an unconfirmed stop refuses to finalize Git; never launch a
        // replacement and never reach the final diff below.
        self.quiesce(state, &session_id).await?;
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
        let outcome = (self.git_finish)(&path, strategy, &base, &branch, &origin).await?;
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
        Ok(())
    }

    /// Waits for an autonomous recommendation's hold to pass, then dispatches it if the
    /// session is still eligible (R10). Tracked in `session.hold_task` separately from `session.tasks`
    /// so that quiescence does not cancel the in-flight switch timer.
    fn schedule_hold_dispatch(&self, state: &mut State, id: &SessionId, plan: HoldDispatchPlan) {
        let HoldDispatchPlan {
            harness,
            hold_s,
            model,
            lane,
            rec_id,
        } = plan;
        let s = match state.session(id) {
            Ok(s) => s,
            Err(_) => return,
        };
        if let Some(prev) = s.hold_task.take() {
            prev.abort();
        }

        let daemon = self.clone();
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(hold_s)).await;
            let eligible = {
                let mut state = daemon.state.lock().await;
                match state.session(&task_id) {
                    Ok(s) => {
                        let target_differs = s.summary.harness != harness
                            || s.summary.model.as_deref() != model.as_deref();
                        let not_no_capacity = !matches!(
                            s.current_recommendation,
                            Some(RouteOutcome::NoCapacity { .. })
                        );
                        let valid = s.summary.mode == Mode::Autonomous
                            && s.summary.active
                            && s.recommendation_id == rec_id
                            && target_differs
                            && not_no_capacity;
                        if valid {
                            daemon.check_capacity(harness, lane.as_deref()).await
                        } else {
                            false
                        }
                    }
                    Err(_) => false,
                }
            };
            if eligible {
                let _ = daemon
                    .switch_and_deliver(&task_id, harness, true, model.as_deref())
                    .await;
            }
        });
        if let Ok(s) = state.session(id) {
            s.hold_task = Some(handle);
        }
    }

    fn add_locked(
        &self,
        state: &mut State,
        repo: PathBuf,
        wt: aihub_git::SessionWorktree,
        harness: HarnessId,
        prompt: Option<String>,
        owner: PrincipalId,
    ) -> Result<SessionId> {
        if state
            .sessions
            .iter()
            .any(|s| s.summary.session_id == wt.session_id)
        {
            bail!("duplicate session");
        }
        let pty = (self.spawner)(harness, options(wt.path.clone(), prompt.clone()), None)?;
        let id = wt.session_id.clone();
        let mut ring = RingBuffer::new(RING_BUFFER_CAP);
        ring.push(&pty.scrollback);
        let (pty_channel_tx, _) = broadcast::channel(256);
        let mut session = Session {
            summary: SessionSummary {
                session_id: id.clone(),
                harness,
                mode: Mode::Assisted,
                repo_path: repo,
                worktree_path: wt.path,
                branch: wt.branch,
                active: true,
                model: None,
                lane: None,
            },
            base: wt.base_branch,
            originating_checkout: wt.originating_checkout,
            prompt,
            pty,
            generation: 0,
            group_stop: GroupStopState::Pending,
            tasks: vec![],
            hold_task: None,
            recommendation_id: 0,
            current_recommendation: None,
            owner,
            ring,
            pty_channel_tx,
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
            channel_ticket: None,
        });
        state.sessions.push(session);
        self.persist_catalog(state);
        Ok(id)
    }

    /// Rewrites `sessions.json` from the current registry (ADR contradição 3, Opção B).
    /// No-op unless `with_sessions_catalog_path` was set. `pid` is always `None`: `aihub-pty`
    /// exposes no PID getter today (see this session's `remaining` in result.md) — the schema
    /// and the startup reconciliation path are real, only the real-PID plumbing is deferred.
    fn persist_catalog(&self, state: &State) {
        let Some(path) = &self.catalog_path else {
            return;
        };
        let mut catalog = sessions_catalog::SessionsCatalog::default();
        for s in &state.sessions {
            catalog.upsert(sessions_catalog::SessionRecord {
                session_id: s.summary.session_id.clone(),
                pid: None,
                owner: s.owner.to_string(),
                repo_path: s.summary.repo_path.clone(),
                worktree_path: s.summary.worktree_path.clone(),
                branch: s.summary.branch.clone(),
                harness: s.summary.harness,
            });
        }
        if let Err(e) = catalog.save(path) {
            log_lifecycle("ERROR", "sessions_catalog_save", &e.to_string());
        }
    }

    /// Bootstrap state for a newly-ticketed PTY channel (`ws.rs`): the live broadcast
    /// subscription, the ring buffer's current head offset and full snapshot, and the
    /// session's input writer — all read under one lock acquisition, so no byte pushed by
    /// `watch()` between reading the snapshot and subscribing can be silently missed. `None`
    /// if the session no longer exists (raced with a purge; §3 "Teste 4" territory).
    pub(crate) async fn pty_channel_bootstrap(
        &self,
        session_id: &SessionId,
    ) -> Option<crate::ws::PtyChannelBootstrap> {
        let state = self.state.lock().await;
        let s = state
            .sessions
            .iter()
            .find(|s| &s.summary.session_id == session_id)?;
        Some(crate::ws::PtyChannelBootstrap {
            rx: s.pty_channel_tx.subscribe(),
            head_offset: s.ring.head_offset(),
            snapshot: s.ring.snapshot(),
            try_write: s.pty.try_write.clone(),
        })
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
                // Ring buffer de 2 MiB indexado por offset monotônico (ADR §2.2, §3),
                // substitui o antigo scrollback de 1 MiB sem numeração de sequência.
                let offset = s.ring.push(&data);
                // Canal de PTY dedicado (ADR contradição 1): mesmos bytes, sem overhead de
                // JSON/Base64. Ignora `SendError` — sem assinante no momento é normal (nenhum
                // cliente de rede anexado ainda).
                let _ = s.pty_channel_tx.send((offset, data.clone()));
                let message = DaemonMessage::PtyOutput {
                    session_id: output_session_id.clone(),
                    data: data.into(),
                    stream_offset: offset,
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
                    // Leader exit alone does not confirm group quiescence (R1).
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
        let daemon = self.clone();
        tasks.spawn(async move {
            daemon.drain_loop().await;
        });
        tokio::pin!(shutdown);
        let result = loop {
            tokio::select! {
                _ = &mut shutdown => break Ok(()),
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => { let daemon = self.clone(); tasks.spawn(async move { let _ = daemon.client(stream, true).await; }); },
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
            if let Some(t) = s.hold_task.take() {
                t.abort();
            }
            for t in s.tasks.drain(..) {
                t.abort();
            }
            if s.group_stop != GroupStopState::Confirmed {
                let stop_fn = s.pty.stop.clone();
                let id = s.summary.session_id.clone();
                if let Err(e) = stop_fn(Duration::from_secs(5)).await {
                    stop_failed = true;
                    s.group_stop = GroupStopState::Unconfirmed;
                    log_lifecycle(
                        "ERROR",
                        "shutdown_stop_failed",
                        &format!("session_id={} stop barrier failed: {e}", id),
                    );
                } else {
                    s.group_stop = GroupStopState::Confirmed;
                }
                s.summary.active = false;
                log_lifecycle(
                    "INFO",
                    "session stop",
                    &format!("session_id={} stopped on shutdown", id),
                );
            }
        }
        if stop_failed {
            bail!("one or more child PTYs could not be stopped");
        }
        log_lifecycle("INFO", "shutdown", "daemon shutdown complete");
        result
    }

    /// Network listener: every connection must prove possession of an allowlisted key before
    /// admission (ADR §2.3, §6) — the opposite trust model of `run()`'s Unix socket, and never
    /// shared with it (ADR §6, "O que isso quebra"). Loopback-only in this slice; a deployable
    /// remote transport (WebSocket, tunnel wiring) is session 04's scope.
    pub async fn run_tcp(
        &self,
        addr: SocketAddr,
        shutdown: impl Future<Output = ()>,
    ) -> Result<()> {
        log_lifecycle("INFO", "start_tcp", &format!("daemon listening on {addr}"));
        let listener = TcpListener::bind(addr).await?;
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        let daemon = self.clone();
                        tasks.spawn(async move { let _ = daemon.client(stream, false).await; });
                    }
                    Err(_) => continue,
                },
                Some(_) = tasks.join_next() => {},
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        log_lifecycle("INFO", "shutdown_tcp", "network listener shutdown complete");
        Ok(())
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
            // N6: refresh the model catalog here, off the registry lock. A hanging
            // model-list executable only delays the next catalog snapshot, never a client.
            for harness in [HarnessId::CursorAgent, HarnessId::Antigravity] {
                if let Some(path) = (self.catalog_paths)(harness) {
                    let current = self.catalog.lock().await.clone();
                    match current.refresh(harness, &path).await {
                        Ok(next) => *self.catalog.lock().await = next,
                        Err(e) => {
                            log_lifecycle("WARN", "catalog_refresh", &format!("{harness}: {e}"))
                        }
                    }
                }
            }
            let requests: Vec<_> = {
                let mut state = self.state.lock().await;
                *self.cache.lock().await = snapshots.clone();
                state.broadcast(DaemonMessage::QuotaPush {
                    snapshots: snapshots.clone(),
                });
                state
                    .sessions
                    .iter()
                    .filter(|s| s.summary.active)
                    .filter_map(|s| s.prompt.clone().map(|p| (s.summary.session_id.clone(), p)))
                    .collect()
            };
            for (id, prompt) in requests {
                let _ = self
                    .recommend(&prompt, Some(id), TaskSize::M, &snapshots)
                    .await;
            }
        }
    }

    /// Periodically retries delivery of anything still spooled, independent of a new handoff
    /// (N1). Entirely off the registry lock: it never touches `self.state`.
    async fn drain_loop(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            match (self.drain)().await {
                Ok(0) => {}
                Ok(n) => log_lifecycle(
                    "INFO",
                    "handoff_drain",
                    &format!("drained {n} spooled handoffs"),
                ),
                Err(e) => log_lifecycle("ERROR", "handoff_drain", &format!("drain failed: {e}")),
            }
        }
    }
    /// Negotiates the handshake for one connection and returns the principal it authenticates
    /// as, or `None` if the connection was already rejected (`DaemonMessage::Unauthorized`
    /// written, caller closes). `is_local` decides which trust model applies (ADR §6):
    /// the Unix socket listener admits v2/uncredentialed clients unconditionally, the network
    /// listener fails closed on anything short of verified proof of possession.
    async fn authenticate<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        is_local: bool,
    ) -> Result<Option<PrincipalId>>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let hello = tokio::time::timeout(Duration::from_secs(10), read_message(reader)).await??;
        let version = match &hello {
            ClientMessage::Hello { version, .. } => *version,
            _ => {
                writer
                    .write_all(&encode_frame(
                        &error("protocol", "expected Hello at connection start").into(),
                    )?)
                    .await?;
                return Ok(None);
            }
        };
        if is_local {
            if version != 2 && version != PROTOCOL_VERSION {
                writer
                    .write_all(&encode_frame(
                        &error("protocol", "unsupported protocol version").into(),
                    )?)
                    .await?;
                return Ok(None);
            }
            writer
                .write_all(&encode_frame(
                    &DaemonMessage::Hello {
                        version: PROTOCOL_VERSION,
                    }
                    .into(),
                )?)
                .await?;
            return Ok(Some(PrincipalId::local()));
        }
        if version != PROTOCOL_VERSION {
            auth::audit_log(
                &self.audit_path,
                "handshake_rejected",
                None,
                &auth::AuthReject::UnsupportedVersion.to_string(),
            );
            writer
                .write_all(&encode_frame(&DaemonMessage::Unauthorized.into())?)
                .await?;
            return Ok(None);
        }
        let mut nonce = [0u8; 32];
        rand::rng().fill_bytes(&mut nonce);
        writer
            .write_all(&encode_frame(
                &DaemonMessage::Challenge {
                    nonce: nonce.to_vec().into(),
                }
                .into(),
            )?)
            .await?;
        let proof = tokio::time::timeout(Duration::from_secs(10), read_message(reader)).await??;
        let credential = match proof {
            ClientMessage::Hello {
                credential: Some(c),
                ..
            } => c,
            _ => {
                auth::audit_log(
                    &self.audit_path,
                    "handshake_rejected",
                    None,
                    &auth::AuthReject::MissingCredential.to_string(),
                );
                writer
                    .write_all(&encode_frame(&DaemonMessage::Unauthorized.into())?)
                    .await?;
                return Ok(None);
            }
        };
        match auth::verify_credential(&credential, &nonce, (self.clock)(), &self.allowlist) {
            Ok(principal) => {
                auth::audit_log(
                    &self.audit_path,
                    "handshake_accepted",
                    Some(&principal),
                    "proof of possession verified",
                );
                writer
                    .write_all(&encode_frame(
                        &DaemonMessage::Hello {
                            version: PROTOCOL_VERSION,
                        }
                        .into(),
                    )?)
                    .await?;
                Ok(Some(principal))
            }
            Err(reason) => {
                auth::audit_log(
                    &self.audit_path,
                    "handshake_rejected",
                    None,
                    &reason.to_string(),
                );
                writer
                    .write_all(&encode_frame(&DaemonMessage::Unauthorized.into())?)
                    .await?;
                Ok(None)
            }
        }
    }

    /// Sends the opaque `Unauthorized` response and records the real reason internally
    /// (ADR §2.3, §6: fail closed, no oracle — the caller never sees `reason`).
    fn reject_unauthorized(
        &self,
        state: &mut State,
        client: u64,
        event: &str,
        principal: Option<&PrincipalId>,
        reason: &str,
    ) {
        auth::audit_log(&self.audit_path, event, principal, reason);
        state.send(client, DaemonMessage::Unauthorized);
    }

    /// Verifies the signed envelope for a pending destructive `MergeRequest` confirmation
    /// (ADR §2.3 nível 3) and finalizes the merge on success. The nonce is one-time use: it was
    /// already removed from `Client::merge_confirm` by the caller before this runs, so a replay
    /// of the same signature never matches a still-pending confirmation again.
    async fn confirm_merge_envelope(
        &self,
        client: u64,
        pending: MergeConfirm,
        credential: &ClientCredential,
    ) {
        let now = (self.clock)();
        let verified = auth::verify_credential(credential, &pending.nonce, now, &self.allowlist);
        let mut state = self.state.lock().await;
        let connection_principal = state.clients.get(&client).map(|c| c.principal.clone());
        let principal = match verified {
            Ok(p) if Some(&p) == connection_principal.as_ref() => p,
            Ok(_) => {
                auth::audit_log(
                    &self.audit_path,
                    "merge_envelope_rejected",
                    None,
                    "envelope key does not match this connection's own principal",
                );
                state.send(client, DaemonMessage::Unauthorized);
                return;
            }
            Err(reason) => {
                auth::audit_log(
                    &self.audit_path,
                    "merge_envelope_rejected",
                    None,
                    &reason.to_string(),
                );
                state.send(client, DaemonMessage::Unauthorized);
                return;
            }
        };
        auth::audit_log(
            &self.audit_path,
            "merge_envelope_verified",
            Some(&principal),
            "one-time nonce consumed, finalizing merge",
        );
        if let Err(e) = self
            .finish_merge(
                &mut state,
                client,
                pending.session_id,
                pending.strategy,
                pending.diff,
                pending.path,
                pending.base,
                pending.branch,
                pending.origin,
            )
            .await
        {
            let response = if e.downcast_ref::<StopUnconfirmed>().is_some() {
                error("stop_unconfirmed", &e.to_string())
            } else {
                error("request_failed", "request could not be completed")
            };
            state.send(client, response);
        }
    }

    async fn client<S>(&self, stream: S, is_local: bool) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let principal = match self
            .authenticate(&mut reader, &mut writer, is_local)
            .await?
        {
            Some(p) => p,
            None => return Ok(()),
        };
        let (id, mut rx) = self.register_client(principal.clone()).await?;
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
            self.dispatch(id, &principal, message).await;
        }
        self.state.lock().await.clients.remove(&id);
        Ok(())
    }

    /// Registers a new connection in `state.clients` and returns its id plus the receiving
    /// half of its outbound message channel — shared by the local Unix socket path (`client`)
    /// and the WebSocket control channel (`ws.rs`), which otherwise duplicate nothing else
    /// about connection bookkeeping.
    pub(crate) async fn register_client(
        &self,
        principal: PrincipalId,
    ) -> Result<(u64, mpsc::Receiver<DaemonMessage>)> {
        let (tx, rx) = mpsc::channel(128);
        let mut state = self.state.lock().await;
        state.next_id += 1;
        let id = state.next_id;
        tx.try_send(DaemonMessage::QuotaPush {
            snapshots: self.cache.lock().await.clone(),
        })?;
        state.clients.insert(
            id,
            Client {
                sender: tx,
                attached: HashSet::new(),
                merge: None,
                principal,
                merge_confirm: None,
            },
        );
        Ok((id, rx))
    }

    /// Dispatches one already-decoded `ClientMessage` for connection `id`/`principal`: the
    /// mid-connection-`Hello`-as-merge-confirmation special case, the session-ownership
    /// pre-check (01-transporte-e-sessao.md §4), and the ordinary `handle()` call — shared by
    /// every transport (`client()` here, and the WebSocket control channel in `ws.rs`).
    pub(crate) async fn dispatch(&self, id: u64, principal: &PrincipalId, message: ClientMessage) {
        // A mid-connection Hello only means something while a destructive MergeRequest
        // envelope is pending (ADR §2.3 nível 3); otherwise it falls through to `handle()`'s
        // ordinary "Hello is only valid at connection start" rejection, unchanged.
        if let ClientMessage::Hello {
            credential: Some(ref cred),
            ..
        } = message
        {
            let pending = self
                .state
                .lock()
                .await
                .clients
                .get_mut(&id)
                .and_then(|c| c.merge_confirm.take());
            if let Some(pending) = pending {
                self.confirm_merge_envelope(id, pending, cred).await;
                return;
            }
        }
        if let Some(sid) = message_session_id(&message) {
            let owned_by_caller = {
                let state = self.state.lock().await;
                match state.sessions.iter().find(|s| &s.summary.session_id == sid) {
                    Some(s) => &s.owner == principal,
                    // Let the ordinary "session not found" error report through `handle()`.
                    None => true,
                }
            };
            if !owned_by_caller {
                let mut state = self.state.lock().await;
                self.reject_unauthorized(
                    &mut state,
                    id,
                    "session_wrong_owner",
                    Some(principal),
                    &format!("touched session {sid} it does not own"),
                );
                return;
            }
        }
        // A disconnect must not cancel a switch after the incoming PTY starts.
        if let Err(e) = self.handle(id, message).await {
            let response = if e.downcast_ref::<StopUnconfirmed>().is_some() {
                error("stop_unconfirmed", &e.to_string())
            } else {
                error("request_failed", "request could not be completed")
            };
            self.state.lock().await.send(id, response);
        }
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
                // CSPRNG, not the old pid-nanos-counter scheme: predictable session ids are
                // enumerable and therefore unsafe as bearer-equivalent handles (ADR §2.2, §4).
                let id = SessionId::generate();
                let owner = state
                    .clients
                    .get(&client)
                    .map(|c| c.principal.clone())
                    .unwrap_or_else(PrincipalId::local);
                let wt = aihub_git::create_session_worktree(&repo_path, &id, None).await?;
                self.add_locked(&mut state, repo_path, wt, harness, initial_prompt, owner)?;
            }
            ClientMessage::Attach {
                target,
                last_seen_offset,
            } => {
                let s = state
                    .sessions
                    .iter()
                    .rev()
                    .find(|s| match &target {
                        SessionTarget::Id(id) => &s.summary.session_id == id,
                        SessionTarget::LatestForRepo(repo) => &s.summary.repo_path == repo,
                    })
                    .ok_or_else(|| anyhow!("session not found"))?;
                let caller = state
                    .clients
                    .get(&client)
                    .map(|c| c.principal.clone())
                    .unwrap_or_else(PrincipalId::local);
                if s.owner != caller {
                    // 01-transporte-e-sessao.md §4: mere possession of `SessionId` confers no
                    // authorization. The caller never learns whose session this is.
                    let owner = s.owner.clone();
                    self.reject_unauthorized(
                        &mut state,
                        client,
                        "attach_wrong_owner",
                        Some(&caller),
                        &format!("tried to attach session owned by {owner}"),
                    );
                    return Ok(());
                }
                let id = s.summary.session_id.clone();
                let summary = s.summary.clone();
                let tail_offset = s.ring.tail_offset();
                // Delta catch-up if the offset is still retained (01-transporte-e-sessao.md
                // §2.2 Caso A); full snapshot + gap flag otherwise, including the `None` a v2
                // client always sends — it has no offset to resume from (Caso B).
                let (scrollback, gap_detected) =
                    match last_seen_offset.and_then(|off| s.ring.delta_since(off)) {
                        Some(delta) => (delta, false),
                        None => (s.ring.snapshot(), true),
                    };
                if let Some(c) = state.clients.get_mut(&client) {
                    c.attached.insert(id.clone());
                }
                // Only network principals need a `channel_ticket`: the local Unix socket keeps
                // carrying both channels over the same connection (ADR §6, contradição 1).
                let channel_ticket = if caller != PrincipalId::local() {
                    Some(state.tickets.issue(id.clone(), caller))
                } else {
                    None
                };
                state.send(
                    client,
                    DaemonMessage::Attached {
                        session_id: id,
                        scrollback: scrollback.into(),
                        summary,
                        stream_offset: tail_offset,
                        gap_detected,
                        channel_ticket,
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
                let s = state.session(&session_id)?;
                let old_mode = s.summary.mode;
                s.summary.mode = mode;
                if old_mode == Mode::Autonomous && mode != Mode::Autonomous {
                    s.recommendation_id += 1;
                    if let Some(t) = s.hold_task.take() {
                        t.abort();
                    }
                }
                state.broadcast(DaemonMessage::ModeSet { session_id, mode });
            }
            ClientMessage::SwitchHarness {
                session_id,
                target,
                with_handoff,
                model,
            } => {
                // Manual /switch is the user's explicit choice: it ignores holds, and drops the
                // registry lock before delivering the handoff so a hung sidecar can't block
                // other clients or shutdown (N1).
                drop(state);
                self.switch_and_deliver(&session_id, target, with_handoff, model.as_deref())
                    .await?;
            }
            ClientMessage::AcceptRecommendation {
                session_id,
                recommendation_id,
            } => {
                let (outcome, current_rec_id) = {
                    let s = state.session(&session_id)?;
                    let outcome = match &s.current_recommendation {
                        Some(o) => o.clone(),
                        None => {
                            state.send(
                                client,
                                error(
                                    "no_recommendation",
                                    &format!(
                                        "session_id={} has no active recommendation",
                                        session_id
                                    ),
                                ),
                            );
                            return Ok(());
                        }
                    };
                    (outcome, s.recommendation_id)
                };
                if let Some(rec_id) = recommendation_id {
                    if rec_id != current_rec_id {
                        state.send(
                            client,
                            error(
                                "stale_recommendation",
                                &format!(
                                    "session_id={} recommendation id {} is stale (current: {})",
                                    session_id, rec_id, current_rec_id
                                ),
                            ),
                        );
                        return Ok(());
                    }
                }
                match outcome {
                    RouteOutcome::NoCapacity { reason } => {
                        state.send(
                            client,
                            error(
                                "no_capacity",
                                &format!("session_id={} has no capacity: {}", session_id, reason),
                            ),
                        );
                    }
                    RouteOutcome::Recommendation {
                        harness,
                        lane,
                        model,
                        holds_until_s,
                    } => {
                        let now = (self.clock)();
                        if holds_until_s.is_some_and(|h| h > now) {
                            state.send(
                                client,
                                error(
                                    "recommendation_held",
                                    &format!(
                                        "session_id={} recommendation is held until epoch {}",
                                        session_id,
                                        holds_until_s.unwrap_or(0)
                                    ),
                                ),
                            );
                            return Ok(());
                        }
                        if !self.check_capacity(harness, lane.as_deref()).await {
                            state.send(
                                client,
                                error(
                                    "no_capacity",
                                    &format!(
                                        "session_id={} target {} has no capacity",
                                        session_id, harness
                                    ),
                                ),
                            );
                            return Ok(());
                        }
                        drop(state);
                        self.switch_and_deliver(&session_id, harness, true, model.as_deref())
                            .await?;
                    }
                }
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
                drop(state);
                self.recommend(&prompt, id, size_hint.unwrap_or(TaskSize::M), &snapshots)
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
                let is_local = state
                    .clients
                    .get(&client)
                    .map(|c| c.principal == PrincipalId::local())
                    .unwrap_or(true);
                if is_local {
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
                        self.finish_merge(
                            &mut state, client, session_id, strategy, diff, path, base, branch,
                            origin,
                        )
                        .await?;
                    }
                } else {
                    // ADR §2.3 nível 3: destrutiva sobre rede exige envelope assinado
                    // independente com nonce de uso único — nunca a mera repetição do
                    // MergeRequest anterior (isso seria só um segundo uso da mesma prova de
                    // posse feita no handshake). O envelope chega como um `Hello.credential`
                    // subsequente na mesma conexão, assinando o nonce abaixo.
                    let mut nonce = [0u8; 32];
                    rand::rng().fill_bytes(&mut nonce);
                    if let Some(c) = state.clients.get_mut(&client) {
                        c.merge_confirm = Some(MergeConfirm {
                            session_id: session_id.clone(),
                            strategy,
                            diff: diff.clone(),
                            nonce: nonce.to_vec(),
                            path,
                            base,
                            branch,
                            origin,
                        });
                    }
                    state.send(client, DaemonMessage::MergeResult {
                        session_id,
                        success: false,
                        diff,
                        message: "Destructive confirmation requires proof of possession: sign the issued nonce and resend Hello to confirm.".into(),
                    });
                    state.send(
                        client,
                        DaemonMessage::Challenge {
                            nonce: nonce.to_vec().into(),
                        },
                    );
                }
            }
            ClientMessage::SubmitTask { session_id, task } => {
                let (snapshots, mode, current_harness, current_model) = {
                    let s = state.session(&session_id)?;
                    s.prompt = Some(task.clone());
                    s.recommendation_id += 1;
                    if let Some(t) = s.hold_task.take() {
                        t.abort();
                    }
                    (
                        self.cache.lock().await.clone(),
                        s.summary.mode,
                        s.summary.harness,
                        s.summary.model.clone(),
                    )
                };
                let classification = (self.classifier)(&task).await;
                let catalog = self.catalog.lock().await.clone();
                let outcome =
                    (self.router)(classification.tier, TaskSize::M, &snapshots, 120, &catalog)?;
                let rec_id = {
                    let s = state.session(&session_id)?;
                    s.current_recommendation = Some(outcome.clone());
                    s.recommendation_id
                };
                state.send(
                    client,
                    DaemonMessage::RouteRecommendation {
                        session_id: session_id.clone(),
                        outcome: outcome.clone(),
                        recommendation_id: rec_id,
                    },
                );
                if mode == Mode::Autonomous {
                    if let RouteOutcome::Recommendation {
                        harness,
                        lane,
                        model,
                        holds_until_s,
                    } = &outcome
                    {
                        let target_differs = *harness != current_harness
                            || current_model.as_deref() != model.as_deref();
                        if target_differs {
                            let now = (self.clock)();
                            let hold_s =
                                holds_until_s.map_or(0, |deadline| deadline.saturating_sub(now));
                            if hold_s == 0 {
                                drop(state);
                                self.switch_and_deliver(
                                    &session_id,
                                    *harness,
                                    true,
                                    model.as_deref(),
                                )
                                .await?;
                                return Ok(());
                            }
                            self.schedule_hold_dispatch(
                                &mut state,
                                &session_id,
                                HoldDispatchPlan {
                                    harness: *harness,
                                    hold_s,
                                    model: model.clone(),
                                    lane: lane.clone(),
                                    rec_id,
                                },
                            );
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
        prompt: &str,
        id: Option<SessionId>,
        size: TaskSize,
        snapshots: &[QuotaSnapshot],
    ) -> Result<()> {
        let classification = (self.classifier)(prompt).await;
        let catalog = self.catalog.lock().await.clone();
        let outcome = (self.router)(classification.tier, size, snapshots, 120, &catalog)?;
        let session_id = id.clone().unwrap_or_else(|| SessionId::new("unknown"));
        let rec_id = {
            let mut state = self.state.lock().await;
            let rec_id = if let Ok(s) = state.session(&session_id) {
                s.recommendation_id += 1;
                s.current_recommendation = Some(outcome.clone());
                if let Some(t) = s.hold_task.take() {
                    t.abort();
                }
                s.recommendation_id
            } else {
                0
            };
            state.broadcast(DaemonMessage::RouteRecommendation {
                session_id: session_id.clone(),
                outcome: outcome.clone(),
                recommendation_id: rec_id,
            });
            rec_id
        };
        if let Some(id) = id {
            let (current_harness, current_model) = {
                let mut state = self.state.lock().await;
                match state.session(&id) {
                    Ok(s) if s.summary.mode == Mode::Autonomous => {
                        (Some(s.summary.harness), s.summary.model.clone())
                    }
                    _ => (None, None),
                }
            };
            if let Some(current_harness) = current_harness {
                if let RouteOutcome::Recommendation {
                    harness,
                    lane,
                    model,
                    holds_until_s,
                } = &outcome
                {
                    let target_differs =
                        *harness != current_harness || current_model.as_deref() != model.as_deref();
                    if target_differs {
                        let now = (self.clock)();
                        let hold_s =
                            holds_until_s.map_or(0, |deadline| deadline.saturating_sub(now));
                        if hold_s == 0 {
                            self.switch_and_deliver(&id, *harness, true, model.as_deref())
                                .await?;
                        } else {
                            let mut state = self.state.lock().await;
                            self.schedule_hold_dispatch(
                                &mut state,
                                &id,
                                HoldDispatchPlan {
                                    harness: *harness,
                                    hold_s,
                                    model: model.clone(),
                                    lane: lane.clone(),
                                    rec_id,
                                },
                            );
                        }
                    }
                }
                // NoCapacity is never dispatched (F10)
            }
        }
        Ok(())
    }

    /// Prepares a switch under the registry lock: quiesces the outgoing harness through the
    /// confirmed stop barrier (F4/F5), extracts and writes the handoff brief, and launches the
    /// incoming harness. Returns the brief and project identity for the caller to deliver
    /// *after* releasing the lock (N1) instead of network-calling memory here.
    pub(crate) async fn switch(
        &self,
        state: &mut State,
        id: &SessionId,
        target: HarnessId,
        handoff: bool,
        model: Option<&str>,
    ) -> Result<Option<(HarnessId, aihub_memory::BriefPair, String)>> {
        let s = state.session(id)?;
        // F5/R8: only same-harness AND same-model AND already running is a no-op. A same-harness
        // switch on a stopped session or with a changed model must relaunch it.
        if s.summary.harness == target && s.summary.model.as_deref() == model && s.summary.active {
            return Ok(None);
        }
        let old = s.summary.harness;
        let wt_path = s.summary.worktree_path.clone();
        let prompt_fallback = s.prompt.clone();
        let generation = s.generation;
        let project = project_identity(&s.originating_checkout);

        // 1. Quiesce outgoing harness through the confirmed stop barrier (F4/F5). An
        // unconfirmed stop refuses to launch the replacement harness.
        self.quiesce(state, id).await?;

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
        let incoming = match (self.spawner)(
            target,
            options(wt_path.clone(), prompt),
            model.map(str::to_string),
        ) {
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
        s.pty = incoming;
        // The ring buffer (and its offset counter) lives on `Session`, not `Pty`: it survives
        // the swap untouched. Only the new harness's initial banner needs pushing into it.
        s.ring.push(&s.pty.scrollback);
        s.generation += 1;
        s.group_stop = GroupStopState::Pending;
        s.summary.harness = target;
        let switched_model = model.map(str::to_string);
        s.summary.model = switched_model.clone();
        s.summary.active = true;
        self.watch(s);

        let handoff_path = brief.as_ref().map(|b| b.brief_path.clone());
        state.broadcast(DaemonMessage::HarnessSwitched {
            session_id: id.clone(),
            old_harness: old,
            new_harness: target,
            handoff_path,
            model: switched_model,
        });
        self.persist_catalog(state);

        match brief {
            Some(b) => Ok(Some((old, b, project))),
            None => {
                log_lifecycle(
                    "INFO",
                    "session switch",
                    &format!(
                        "session_id={} from={} to={} handoff_dest=none",
                        id, old, target
                    ),
                );
                Ok(None)
            }
        }
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
/// Session a `ClientMessage` already addresses, for the ownership pre-check in `client()`
/// (01-transporte-e-sessao.md §4). `Attach` is checked separately inside `handle()` itself,
/// since `SessionTarget::LatestForRepo` has no `session_id` to inspect up front.
fn message_session_id(message: &ClientMessage) -> Option<&SessionId> {
    match message {
        ClientMessage::Detach { session_id }
        | ClientMessage::PtyInput { session_id, .. }
        | ClientMessage::PtyResize { session_id, .. }
        | ClientMessage::SetMode { session_id, .. }
        | ClientMessage::SwitchHarness { session_id, .. }
        | ClientMessage::MergeRequest { session_id, .. }
        | ClientMessage::SubmitTask { session_id, .. }
        | ClientMessage::AcceptRecommendation { session_id, .. } => Some(session_id),
        _ => None,
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
    use tokio::net::UnixStream;

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
        let daemon = Daemon::new(|| async { vec![] }, |_, _, _| panic!("no spawn"));
        let mut workers = JoinSet::new();
        let mut clients = vec![];
        for _ in 0..2 {
            let (mut client, server) = UnixStream::pair().unwrap();
            let d = daemon.clone();
            workers.spawn(async move {
                d.client(server, true).await.unwrap();
            });
            client
                .write_all(
                    &encode_frame(
                        &ClientMessage::Hello {
                            version: PROTOCOL_VERSION,
                            credential: None,
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
            move |harness, opts, _model| {
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
            .switch(&mut state, &id, HarnessId::ClaudeCode, false, None)
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
            |_, _, _| panic!("no PTY requested"),
        );
        let (tx, mut rx) = mpsc::channel(16);
        daemon.state.lock().await.clients.insert(
            1,
            Client {
                sender: tx,
                attached: HashSet::new(),
                merge: None,
                principal: PrincipalId::local(),
                merge_confirm: None,
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
