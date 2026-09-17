# aihub Architecture & API Contract

This document defines the strict public API contract for all 8 member crates in the `aihub` Cargo workspace.
Sessions 02 through 09 implement these interfaces in parallel without inter-session communication; all implementations must conform precisely to these signatures and specifications.

---

## 1. IPC Protocol Framing & Serialization (`aihub-core`)

> **Encoding Choice Justification:**
> Base64-in-JSON was chosen over binary serde formats to keep IPC message inspection, debugging, and cross-session framing uniform and human-readable with standard JSON tooling while keeping PTY payloads compact.

### Framing Specification
- **Transport:** Unix Domain Socket at `~/.local/share/aihub/aihub.sock` (or custom path passed to path helpers).
- **Framing:** 4-byte big-endian unsigned length prefix (`u32`) preceding a UTF-8 JSON payload.
- **Maximum Frame Length:** 32 MiB (`DEFAULT_MAX_FRAME_LENGTH = 33_554_432`).
- **Initial Handshake:** The first frame in both directions MUST be `Hello { version: 2 }`.
- **PTY Streams:** Binary data transmitted in `PtyInput`, `PtyOutput`, and `Attached.scrollback` is wrapped in `Base64Bytes`, serializing directly to a base64 string rather than a JSON array of integers.

---

## 2. Public Signatures by Crate

### 2.1. `aihub-core` (Owned by Session 01)
Full implementation provided. Defines domain types, quota models, IPC messages, framing codecs, and filesystem path helpers.

#### Module `aihub_core::types`
```rust
pub struct SessionId(pub String);
impl SessionId {
    pub fn new(id: impl Into<String>) -> Self;
    pub fn as_str(&self) -> &str;
    pub fn branch_name(&self) -> String; // "session/<session-id>"
    pub fn worktree_path(&self, worktree_root: &Path) -> PathBuf; // "<root>/<session-id>"
}

pub enum HarnessId {
    ClaudeCode,
    Antigravity,
    Codex,
    CursorAgent,
}
impl HarnessId {
    pub fn binary_name(&self) -> &'static str; // "claude", "agy", "codex", "cursor-agent"
    pub fn from_binary_name(name: &str) -> Option<Self>;
    pub fn all() -> &'static [HarnessId];
}

pub enum TaskTier {
    Mechanical,
    Design,
    Review,
}

pub enum TaskSize {
    S,
    M,
    L,
}

pub enum Mode {
    Assisted,
    Autonomous,
}

pub enum MergeStrategy {
    Squash,
    FastForward,
    Keep,
    Discard,
}

pub enum SessionTarget {
    Id(SessionId),
    LatestForRepo(PathBuf),
}

pub struct SessionSummary {
    pub session_id: SessionId,
    pub harness: HarnessId,
    pub mode: Mode,
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub active: bool,
}

pub enum RouteOutcome {
    Recommendation {
        harness: HarnessId,
        lane: Option<String>,
        model: Option<String>,
        /// Absolute Unix-epoch seconds after which the hold expires (session 13 / R3), not a
        /// duration. Router computes it as `now_s + hold_duration_s` at recommendation time
        /// (`aihub-router/src/lib.rs:219`); consumers compare it directly against the current
        /// epoch time, never against 0.
        holds_until_s: Option<u64>,
    },
    NoCapacity {
        reason: String,
    },
}
pub type RoutingOutcome = RouteOutcome;
```

#### Module `aihub_core::quota`
```rust
pub struct SlotId {
    pub harness: HarnessId,
    pub account: String,
}
impl SlotId {
    pub fn new(harness: HarnessId, account: impl Into<String>) -> Self;
    pub fn default_for(harness: HarnessId) -> Self;
    pub fn key(&self) -> String; // "<binary>:<account>"
}

pub enum WindowKind {
    FiveHour,
    SevenDay,
    Cycle,
    Custom(String),
}

/// NOTE: `used_pct` represents PERCENTAGE USED (0.0 to 100.0), NOT remaining.
/// Porters converting from handoff.mjs must compute: `used_pct = 100.0 - remaining_pct`.
pub struct QuotaWindow {
    pub kind: WindowKind,
    pub used_pct: f64,
    pub resets_in_s: Option<u64>,
    pub window_s: Option<u64>,
}
impl QuotaWindow {
    pub fn new(kind: WindowKind, used_pct: f64, resets_in_s: Option<u64>, window_s: Option<u64>) -> Self;
    pub fn remaining_pct(&self) -> f64;
}

pub enum LaneKind {
    Own,      // e.g. Cursor Models (Auto/Composer), Antigravity Gemini
    Frontier, // e.g. Cursor Other Models (Claude/GPT), Antigravity 3p
}

pub struct QuotaLane {
    pub name: String,
    pub kind: LaneKind,
    pub windows: Vec<QuotaWindow>,
}

pub enum QuotaStatus {
    Ok,
    Low,
    Empty,
    Unknown,
}

pub enum QuotaSource {
    Vendor,
    OAuth,
    Transcript,
}

/// NOTE: Never contains passwords, auth tokens, or credentials.
pub struct QuotaSnapshot {
    pub slot: SlotId,
    pub status: QuotaStatus,
    pub source: QuotaSource,
    pub estimated: bool,
    pub note: Option<String>,
    pub windows: Vec<QuotaWindow>,
    pub lanes: Vec<QuotaLane>,
}
impl QuotaSnapshot {
    pub fn tightest_window(&self) -> Option<&QuotaWindow>;
    pub fn remaining_pct(&self) -> Option<f64>;
    pub fn is_available(&self) -> bool;
}
```

#### Module `aihub_core::ipc`

**PROTOCOL_VERSION stays 2 (session 13 decision, not a bump).** Sessions 09–12 added three
wire fields — `ClientMessage::AcceptRecommendation`, `DaemonMessage::RouteRecommendation
.recommendation_id`, `DaemonMessage::HarnessSwitched.model` — and changed what
`RouteOutcome::Recommendation.holds_until_s` means (duration → absolute epoch deadline, R3).
The three added fields carry `#[serde(default)]` and decode against an old peer. The
`holds_until_s` semantic change has no such guard: a peer that still reads it as a duration
would reproduce R3 (compares an epoch against `now`, decides the hold is already expired).
That is not survivable via `#[serde(default)]` — it is a same-shape, different-meaning field.
Bumping `PROTOCOL_VERSION` would not fix it either, since `perform_handshake`
(`aihub/src/connection.rs:186`) refuses the connection outright on any mismatch rather than
negotiating a compatibility mode; a bump only changes which exact version string must match,
it does not let an old client interpret the new field correctly.
The actual reason a mismatched pair cannot occur: `scripts/install.sh` always installs
`aihub` and `aihubd` from the same build in one invocation (`--bin-dir` or
`cargo install --path` against the same checkout, `install.sh:178-183`) — there is no
supported path to upgrade one binary independently of the other. Given that constraint,
`PROTOCOL_VERSION` correctly stays 2: the field is additive from a decode-compatibility
standpoint, and the semantic change is safe only because — not despite — the daemon and TUI
never ship or run at different versions against each other in this project's install model.
If that install model ever changes (e.g. a daemon that outlives TUI upgrades, or a
remote/detached daemon per `design/remote-split`), this decision must be revisited and
`PROTOCOL_VERSION` bumped at that point.

```rust
pub const PROTOCOL_VERSION: u32 = 2;

pub struct Base64Bytes(pub Vec<u8>);
impl Base64Bytes {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self;
    pub fn as_slice(&self) -> &[u8];
    pub fn into_inner(self) -> Vec<u8>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}

pub enum ClientMessage {
    Hello { version: u32 },
    ListSessions,
    NewSession {
        harness: HarnessId,
        repo_path: PathBuf,
        initial_prompt: Option<String>,
    },
    Attach { target: SessionTarget },
    Detach { session_id: SessionId },
    PtyInput { session_id: SessionId, data: Base64Bytes },
    PtyResize { session_id: SessionId, cols: u16, rows: u16 },
    RequestQuota,
    RouteRequest {
        prompt: String,
        repo_path: Option<PathBuf>,
        size_hint: Option<TaskSize>,
    },
    SwitchHarness {
        session_id: SessionId,
        target: HarnessId,
        with_handoff: bool,
        /// Recommended CLI model id when accepting a route recommendation; omitted on manual `/switch`.
        model: Option<String>, // `#[serde(default)]` — absent on older clients
    },
    SetMode { session_id: SessionId, mode: Mode },
    MergeRequest { session_id: SessionId, strategy: MergeStrategy },
    SubmitTask { session_id: SessionId, task: String },
    /// Accept the current assisted recommendation for a session (session 12 / R3). New in this
    /// round; TUI sends it from `aihub/src/keys.rs:191` instead of a manual `SwitchHarness` so
    /// the daemon can validate acceptance against its own held recommendation and deadline.
    AcceptRecommendation {
        session_id: SessionId,
        #[serde(default)]
        recommendation_id: Option<u64>, // `#[serde(default)]` — absent on older clients
    },
}

pub enum DaemonMessage {
    Hello { version: u32 },
    SessionList { sessions: Vec<SessionSummary> },
    SessionCreated {
        session_id: SessionId,
        harness: HarnessId,
        worktree_path: PathBuf,
        branch: String,
    },
    Attached {
        session_id: SessionId,
        scrollback: Base64Bytes,
        summary: SessionSummary,
    },
    Detached { session_id: SessionId },
    PtyOutput { session_id: SessionId, data: Base64Bytes },
    SessionExited { session_id: SessionId, exit_code: Option<i32> },
    QuotaPush { snapshots: Vec<QuotaSnapshot> },
    RouteRecommendation {
        session_id: SessionId,
        outcome: RouteOutcome,
        /// New in session 12 / R10: identifies which recommendation an `AcceptRecommendation`
        /// or hold-expiry dispatch is acting on, so a stale hold timer can't act on newer
        /// context. `#[serde(default)]` — 0 on older daemons/clients.
        #[serde(default)]
        recommendation_id: u64,
    },
    HarnessSwitched {
        session_id: SessionId,
        old_harness: HarnessId,
        new_harness: HarnessId,
        handoff_path: Option<PathBuf>,
        /// New in session 12 / R8: the model that was actually passed to the spawned harness,
        /// so lane/model enforcement is visible to the client. `#[serde(default)]`.
        #[serde(default)]
        model: Option<String>,
    },
    ModeSet { session_id: SessionId, mode: Mode },
    MergeResult {
        session_id: SessionId,
        success: bool,
        diff: String,
        message: String,
    },
    Error { code: String, message: String },
}

pub enum IpcMessage {
    Client(ClientMessage),
    Daemon(DaemonMessage),
}
```

#### Module `aihub_core::codec`
```rust
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 32 * 1024 * 1024;

pub enum IpcCodecError {
    Io(std::io::Error),
    Json(serde_json::Error),
    FrameTooLarge { length: usize, max: usize },
}

pub struct IpcCodec { /* ... */ }
impl IpcCodec {
    pub fn new(max_frame_length: usize) -> Self;
    pub fn max_frame_length(&self) -> usize;
}
impl tokio_util::codec::Encoder<IpcMessage> for IpcCodec;
impl tokio_util::codec::Decoder for IpcCodec;

pub fn encode_frame(msg: &IpcMessage) -> Result<Vec<u8>, IpcCodecError>;
pub fn decode_frame(buf: &[u8]) -> Result<Option<(IpcMessage, usize)>, IpcCodecError>;
```

#### Module `aihub_core::paths`
```rust
pub fn default_data_dir() -> PathBuf; // ~/.local/share/aihub
pub fn default_socket_path() -> PathBuf; // ~/.local/share/aihub/aihub.sock
pub fn socket_path_in(data_dir: &Path) -> PathBuf;
pub fn default_worktree_root() -> PathBuf; // ${TMPDIR}/aihub/worktrees
pub fn worktree_root_in(tmp_dir: &Path) -> PathBuf;
pub fn session_worktree_dir(worktree_root: &Path, session_id: &SessionId) -> PathBuf;
pub fn session_branch_name(session_id: &SessionId) -> String; // "session/<session-id>"
```

---

### 2.2. `aihub-probe` (Owned by Sessions 02 & 03; Wiring in `lib.rs` by Session 01)

#### `aihub-probe/src/lib.rs` (Session 01)
```rust
pub enum ProbeError {
    Io(std::io::Error),
    Http(reqwest::Error),
    Database(rusqlite::Error),
    Json(serde_json::Error),
    Failure(String),
}

/// Dispatches all probe modules concurrently via tokio::join! and aggregates snapshots.
pub async fn probe_all() -> Vec<QuotaSnapshot>;
```

#### `aihub-probe/src/claude.rs` (Owned by Session 02)
```rust
pub async fn probe() -> Result<QuotaSnapshot, ProbeError>;
pub fn parse_claude_usage(json: &str) -> Result<Vec<QuotaWindow>, ProbeError>;
```

#### `aihub-probe/src/codex.rs` (Owned by Session 02)
```rust
pub async fn probe() -> Result<QuotaSnapshot, ProbeError>;
pub fn parse_codex_usage(json: &str) -> Result<Vec<QuotaWindow>, ProbeError>;
```

#### `aihub-probe/src/transcripts.rs` (Owned by Session 02)
```rust
pub struct TranscriptTurn {
    pub timestamp_s: u64,
    pub tokens_used: u64,
    pub session_id: String,
}

pub async fn probe() -> Result<Vec<QuotaSnapshot>, ProbeError>;
pub fn parse_transcript_line(line: &str) -> Result<Option<TranscriptTurn>, ProbeError>;
pub fn calculate_rolling_windows(turns: &[TranscriptTurn]) -> Vec<QuotaWindow>;
```

#### `aihub-probe/src/windows.rs` (Owned by Session 02)
```rust
pub fn compute_window_resets(start_epoch_s: u64, duration_s: u64) -> Option<u64>;
pub fn pick_tightest_window<'a>(windows: &'a [QuotaWindow]) -> Option<&'a QuotaWindow>;
pub fn bucket_for_usage(used_pct: f64, low_threshold_pct: f64) -> QuotaStatus;
```

#### `aihub-probe/src/cursor.rs` (Owned by Session 03)
```rust
pub async fn probe() -> Result<QuotaSnapshot, ProbeError>;
pub fn parse_dashboard_usage(json: &str) -> Result<(Vec<QuotaWindow>, Vec<QuotaLane>), ProbeError>;
pub fn parse_summary_usage(json: &str) -> Result<(Vec<QuotaWindow>, Vec<QuotaLane>), ProbeError>;
pub fn extract_cursor_jwt(json: &str) -> Option<String>;
```

#### `aihub-probe/src/antigravity.rs` (Owned by Session 03)
```rust
pub async fn probe() -> Result<QuotaSnapshot, ProbeError>;
pub fn parse_quota_summary(json: &str) -> Result<Vec<QuotaLane>, ProbeError>;
pub fn parse_lsof_ports(lsof_output: &str) -> Vec<u16>;
pub fn parse_csrf_token(html: &str) -> Option<String>;
```

---

### 2.3. `aihub-router` (Owned by Session 04)

```rust
pub enum RouterError {
    Http(reqwest::Error),
    Json(serde_json::Error),
    RoutingFailed(String),
}

pub struct Classification {
    pub tier: TaskTier,
    pub confidence: f32,
    pub ambiguous: bool,
}

/// Pure regex / verb heuristics (<2ms) to classify prompt into a task tier.
pub fn classify(prompt: &str) -> Classification;

/// Fallback LLM prompt classification for ambiguous or lengthy tasks.
pub async fn classify_with_llm(prompt: &str, api_key: Option<&str>) -> Result<Classification, RouterError>;

```

---

### 2.4. `aihub-pty` (Owned by Session 05)

```rust
pub enum PtyError {
    Io(std::io::Error),
    Pty(String),
    NotRunning,
}

pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

pub struct PtySpawnOptions {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub size: PtySize,
    pub initial_prompt: Option<String>,
}

pub struct PtyHandle { /* fields managed by session 05 */ }
impl PtyHandle {
    pub fn resize(&self, size: PtySize) -> Result<(), PtyError>;
    pub fn subscribe_output(&self) -> tokio::sync::broadcast::Receiver<Vec<u8>>;
    pub fn scrollback_snapshot(&self) -> Vec<u8>;
    pub async fn wait(&self) -> Result<Option<i32>, PtyError>;
    pub async fn kill(&self) -> Result<(), PtyError>;
}

pub fn spawn_command(cmd: &str, args: &[&str], opts: PtySpawnOptions) -> Result<PtyHandle, PtyError>;
pub fn spawn_harness(
    harness: HarnessId,
    opts: PtySpawnOptions,
    model: Option<&str>,
) -> Result<PtyHandle, PtyError>;

pub struct HarnessLaunchRecipe {
    pub binary: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

pub fn harness_recipe(harness: HarnessId, initial_prompt: Option<&str>) -> HarnessLaunchRecipe;
```

---

### 2.5. `aihub-git` (Owned by Session 06)

```rust
pub enum GitError {
    Io(std::io::Error),
    CommandFailed(String),
    InvalidRepo(String),
}

pub struct SessionWorktree {
    pub session_id: SessionId,
    pub path: PathBuf,
    pub branch: String,
    pub base_branch: String,
}

pub struct MergeOutcome {
    pub strategy: MergeStrategy,
    pub success: bool,
    pub diff: String,
    pub message: String,
}

/// Spawns git subprocesses to create worktree branch `session/<id>` at `${TMPDIR}/aihub/worktrees/<id>`.
pub async fn create_session_worktree(
    repo_path: &Path,
    session_id: &SessionId,
    worktree_root: Option<&Path>,
) -> Result<SessionWorktree, GitError>;

/// Generates diff against the base branch.
pub async fn diff(worktree_path: &Path, base_branch: &str, colored: bool) -> Result<String, GitError>;

```

---

### 2.6. `aihub-memory` (Owned by Session 07)

```rust
pub enum MemoryError {
    Io(std::io::Error),
    Database(rusqlite::Error),
    ExtractionFailed(String),
    Serialization(serde_json::Error),
}

pub struct HandoffTurn {
    pub summary: String,
    pub last_output: String,
    pub decisions: Vec<String>,
}

pub struct BriefPair {
    pub brief_path: PathBuf,
    pub prompt_path: PathBuf,
}

/// Extracts final decisions and context from the exiting harness's turn logs.
pub async fn extract_last_turn(
    session_id: &SessionId,
    harness: HarnessId,
    worktree_path: &Path,
) -> Result<HandoffTurn, MemoryError>;

/// Writes the handoff brief pair (`NN.md`, `NN.prompt.md`).
pub fn write_brief_pair(
    output_dir: &Path,
    session_index: u32,
    goal: &str,
    turn: &HandoffTurn,
) -> Result<BriefPair, MemoryError>;

/// Persists the handoff metadata and brief reference to `ai-memory` SQLite/Git store.
pub async fn record_handoff(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
) -> Result<(), MemoryError>;
```

---

### 2.7. `aihubd` (Owned by Session 08)

- **Binary Name:** `aihubd`
- **Entry Point:** `aihubd/src/main.rs`
- **Ownership:** Session 08 designs and implements daemon internals:
  - Unix Domain Socket server (`~/.local/share/aihub/aihub.sock`).
  - Session registry mapping `SessionId` to PTY instances and git worktrees.
  - Background telemetry loop updating quota snapshots every 2 minutes or on-demand.
  - Multi-client attachment and broadcast multiplexing.

---

### 2.8. `aihub` (Owned by Session 09)

- **Binary Name:** `aihub`
- **Entry Point:** `aihub/src/main.rs`
- **Ownership:** Session 09 designs and implements TUI client internals:
  - Ratatui / Crossterm interface with statusline, virtual terminal PTY viewport, and command palette.
  - Interactive commands: `/switch`, `/merge`, `/quota`, `Ctrl+P`, `Ctrl+M`.
  - ANSI streaming from `aihubd` over Unix domain socket.

---

## 3. Production Round Contract Extensions

These new public signatures are added additively for production readiness, each owned and implemented by its respective parallel session:

### 3.1. `aihub-pty` (Owned by Session 03)

- **Stop barrier (Finding F4):** Terminate the child's process group, wait up to a timeout, escalate to kill, reap, and return the exit status.
  ```rust
  impl PtyHandle {
      pub async fn stop(&self, timeout: std::time::Duration) -> Result<Option<i32>, PtyError>;
      pub async fn stop_barrier(&self, timeout: std::time::Duration) -> Result<Option<i32>, PtyError>;
  }
  ```
- **Non-blocking input path (Finding F7):** A bounded queue drained by a writer thread, where a full queue returns an error.
  ```rust
  pub enum PtyError {
      /* existing variants: Io, Pty, NotRunning */
      QueueFull,
  }

  impl PtyHandle {
      pub fn try_write(&self, data: &[u8]) -> Result<(), PtyError>;
      pub fn write_nonblocking(&self, data: &[u8]) -> Result<(), PtyError>;
  }
  ```
- **Launch recipe with model identifier (Plan §3.3):**
  ```rust
  pub fn harness_recipe_with_model(
      harness: HarnessId,
      initial_prompt: Option<&str>,
      model: Option<&str>,
  ) -> HarnessLaunchRecipe;
  ```

### 3.2. `aihub-router` (Owned by Session 05)

- **Typed outcome routing (Finding F10):** `route` returning the typed outcome (`RouteOutcome`) from core.
  ```rust
  pub fn route_outcome(
      tier: TaskTier,
      size: TaskSize,
      snapshots: &[QuotaSnapshot],
      horizon_s: u64,
      catalog: &ModelCatalog,
  ) -> Result<RouteOutcome, RouterError>;

  pub fn route_typed(
      tier: TaskTier,
      size: TaskSize,
      snapshots: &[QuotaSnapshot],
      horizon_s: u64,
      catalog: &ModelCatalog,
  ) -> Result<RouteOutcome, RouterError>;
  ```
- **Async classify-with-fallback (Finding F9):** Entry the daemon calls for long or ambiguous prompts.
  ```rust
  pub async fn classify_with_fallback(prompt: &str) -> Classification;
  ```
- **Lane to model-id helper (Plan §3.3):** Maps harness and lane to recommended CLI model identifier for the launch recipe.
  ```rust
  pub fn lane_to_model_id(harness: HarnessId, lane: &str, catalog: &ModelCatalog) -> Option<String>;
  pub fn model_for_lane(harness: HarnessId, lane: Option<&str>, catalog: &ModelCatalog) -> Option<String>;

  /// Immutable snapshot of CLI `--model` lists; refreshed off the daemon lock (N6).
  pub struct ModelCatalog { /* … */ }
  impl ModelCatalog {
      pub fn refresh(&self, harness: HarnessId, executable: &Path) -> impl Future<Output = Result<Self, CatalogError>>;
      pub fn model_for_lane(&self, harness: HarnessId, lane: Option<&str>) -> Option<String>;
  }
  pub enum CatalogError { /* Timeout, OutputLimit, Io, Failed, InvalidOutput, UnsupportedHarness */ }
  ```

### 3.3. `aihub-git` (Owned by Session 02)

- **Originating checkout and session branch validation (Findings F3, F14):**
  The creation result records both values, and the finish API validates both.
  ```rust
  pub struct SessionWorktree {
      pub session_id: SessionId,
      pub path: PathBuf,
      pub branch: String,
      pub base_branch: String,
      pub originating_checkout: PathBuf,
  }

  pub async fn finish_session(
      worktree_path: &Path,
      strategy: MergeStrategy,
      base_branch: &str,
      expected_session_branch: &str,
      originating_checkout: &Path,
  ) -> Result<MergeOutcome, GitError>;

  pub async fn finish_validated(
      worktree_path: &Path,
      strategy: MergeStrategy,
      base_branch: &str,
      expected_session_branch: &str,
      originating_checkout: &Path,
  ) -> Result<MergeOutcome, GitError>;
  ```

### 3.4. `aihub-memory` (Owned by Session 06)

- **Handoff backend boundary (§3.5, Findings F1, F11):** One function records a handoff and returns whether it reached the live backend or was spooled locally. Names outside `aihub-memory/src/ai_memory.rs` are backend-neutral — the concrete backend (currently ai-memory) is that one file's implementation detail (session 10 correction pass).
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
  pub enum HandoffDestination {
      Delivered,
      Spooled,
  }
  impl HandoffDestination {
      pub fn is_delivered(&self) -> bool;
      pub fn is_spooled(&self) -> bool;
  }

  pub async fn record_handoff_destination(
      session_id: &SessionId,
      from: HarnessId,
      to: HarnessId,
      brief: &BriefPair,
  ) -> Result<HandoffDestination, MemoryError>;

  pub async fn record_handoff_delivered(
      session_id: &SessionId,
      from: HarnessId,
      to: HarnessId,
      brief: &BriefPair,
  ) -> Result<bool, MemoryError>;

  /// Explicit-parameter form (never reads env); record_handoff_destination's
  /// implementation, exported so tests and scripts/e2e-ai-memory.sh never
  /// need std::env::set_var.
  pub async fn record_handoff_to(
      session_id: &SessionId,
      from: HarnessId,
      to: HarnessId,
      brief: &BriefPair,
      server_url: &str,
      auth_token: Option<&str>,
      data_dir: &Path,
  ) -> Result<HandoffDestination, MemoryError>;
  ```

---

## 4. Removed after production integration (session 10)

These pre–production-round entry points were superseded in sessions 02–09 and removed once no caller remained:

| Crate | Removed | Replacement |
|-------|---------|-------------|
| `aihub-pty` | Blocking `PtyHandle::write(&[u8])` on the PTY writer thread | `try_write` / `write_nonblocking` (bounded queue, `PtyError::QueueFull`) |
| `aihub-router` | `route(...) -> Result<HarnessId, RouterError>` (untyped recommendation) | `route_outcome` / `route_typed` → `RouteOutcome` (`Recommendation` or `NoCapacity`) |
| `aihub-git` | `finish(worktree, strategy, base_branch)` (branch discovered from checkout) | `finish_session` / `finish_validated` with `expected_session_branch` and `originating_checkout` |

## 5. Renamed for backend neutrality (session 10 correction pass)

| Crate | Old | New |
|-------|-----|-----|
| `aihub-memory` | `HandoffDestination::AiMemory` | `HandoffDestination::Delivered` |
| `aihub-memory` | `HandoffDestination::SpooledLocally` | `HandoffDestination::Spooled` |
| `aihub-memory` | `HandoffDestination::reached_ai_memory()` | `HandoffDestination::is_delivered()` |
| `aihub-memory` | `HandoffDestination::was_spooled_locally()` | `HandoffDestination::is_spooled()` |

All callers in `aihubd` and its tests were updated in the same pass; no behavior changed.

---

## 6. Handoff run 20260915T225713Z (sessions 01–04 integrator notes)

Additive changes landed in this production round; session 07 wired the TUI to the IPC field below.

### 6.1 `aihub-pty` (session 01)

- `spawn_harness(..., model: Option<&str>)` — optional `--model` via `harness_recipe_with_model`.
- `PtyHandle::stop` / `stop_barrier` — process-group termination with timeout, kill escalation, and typed `PtyError` (`StopTimeout`, `StopSignal`, `StopReap`, `QueueFull` on non-blocking input).

### 6.2 `aihub-memory` (session 02)

- Handoff recording takes **repository identity** (`project: &str`) at every public entry point; callers must pass stable repo id, not ephemeral worktree/session dir names.
- `SpooledRecord`, `MemoryError::SpoolFull` / `DeliveryFailed`, bounded MCP delivery (5s, 1 MiB cap), atomic spool rewrite with `File::lock`.
- `drain_spooled_handoffs` / `drain_spooled_handoffs_to` — timer-driven retry without a new handoff.

### 6.3 `aihub-router` / `aihub-probe` (session 03)

- Routing and lane helpers require `&ModelCatalog`; refresh is async, bounded, and must not run under the daemon registry lock.
- `discover_ls_bases_from(override_addr, lsof_output)` — pure antigravity LS discovery for tests (production wrapper may shell out to `lsof`).

### 6.4 `aihub-core` / `aihubd` (session 04)

- `ClientMessage::SwitchHarness.model: Option<String>` — threads recommendation model id to spawn; TUI sets it on Enter-to-accept after hold expiry.
- `aihubd::spawn_pty(..., model: Option<String>)` and daemon seams `with_catalog_paths`, `with_drain`, extended `with_router` / `with_memory_recorder` closures (see session 04 result `api:` block).

## 7. Handoff run 20260915T225713Z (sessions 09–12, integrated by session 13)

Additive wire changes from the R1–R10 remediation round (`docs/reviews/2026-09-16-review-run3.md`).
All four are additive/`#[serde(default)]`-guarded except the `holds_until_s` semantic change,
whose safety rests on the install-pairing argument in §2.1 `aihub_core::ipc` above, not on a
serde default. See that section for the full `PROTOCOL_VERSION` reasoning.

- `ClientMessage::AcceptRecommendation { session_id, recommendation_id: Option<u64> }` — new
  (`aihub-core/src/ipc.rs:144`). TUI sends it from `aihub/src/keys.rs:191` on Enter-to-accept or
  hold-expiry auto-accept, instead of a manual `SwitchHarness`; daemon handles it at
  `aihubd/src/lib.rs:1268` and validates it against its own held recommendation (R3).
- `DaemonMessage::RouteRecommendation.recommendation_id: u64` — new (`ipc.rs:191`), `#[serde(default)]`.
  Identifies the recommendation an `AcceptRecommendation` or hold-timer dispatch is acting on, so a
  stale hold can't fire against newer context (R10).
- `DaemonMessage::HarnessSwitched.model: Option<String>` — new (`ipc.rs:200`), `#[serde(default)]`.
  Reports the model that was actually passed to `spawn_pty` for the new harness (R8).
- `RouteOutcome::Recommendation.holds_until_s` — **semantic change, no wire-shape change**: was a
  duration in seconds from the router; is now an absolute Unix-epoch deadline
  (`aihub-router/src/lib.rs:219`, `hold.map(|h| now_s + h)`). Every consumer (`aihub/src/keys.rs:183`,
  `aihub/src/ui/footer.rs:38`, `aihubd/src/lib.rs:1324,1495,1581`) compares it against "now" or
  computes a remaining duration by subtracting "now" from it — never against 0 — closing R3.

