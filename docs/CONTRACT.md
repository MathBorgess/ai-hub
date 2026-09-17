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
- **Initial Handshake:** The first frame in both directions MUST be `Hello { version: 1 }`.
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
```rust
pub const PROTOCOL_VERSION: u32 = 1;

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
    },
    SetMode { session_id: SessionId, mode: Mode },
    MergeRequest { session_id: SessionId, strategy: MergeStrategy },
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
    Attached { session_id: SessionId, scrollback: Base64Bytes },
    Detached { session_id: SessionId },
    PtyOutput { session_id: SessionId, data: Base64Bytes },
    SessionExited { session_id: SessionId, exit_code: Option<i32> },
    QuotaPush { snapshots: Vec<QuotaSnapshot> },
    RouteRecommendation {
        tier: TaskTier,
        harness: HarnessId,
        lane: Option<String>,
        holds_until_s: Option<u64>,
        confidence: f32,
        reason: String,
    },
    HarnessSwitched {
        session_id: SessionId,
        old_harness: HarnessId,
        new_harness: HarnessId,
        handoff_path: Option<PathBuf>,
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

pub struct Recommendation {
    pub tier: TaskTier,
    pub harness: HarnessId,
    pub lane: Option<String>,
    pub holds_until_s: Option<u64>,
    pub reason: String,
}

/// Pure regex / verb heuristics (<2ms) to classify prompt into a task tier.
pub fn classify(prompt: &str) -> Classification;

/// Fallback LLM prompt classification for ambiguous or lengthy tasks.
pub async fn classify_with_llm(prompt: &str, api_key: Option<&str>) -> Result<Classification, RouterError>;

/// Assigns optimal harness, lane, and hold duration based on task tier, size, quota snapshots, and horizon.
pub fn route(
    tier: TaskTier,
    size: TaskSize,
    snapshots: &[QuotaSnapshot],
    horizon_s: u64,
) -> Result<Recommendation, RouterError>;
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
    pub async fn write(&self, data: &[u8]) -> Result<(), PtyError>;
    pub fn resize(&self, size: PtySize) -> Result<(), PtyError>;
    pub fn subscribe_output(&self) -> tokio::sync::broadcast::Receiver<Vec<u8>>;
    pub fn scrollback_snapshot(&self) -> Vec<u8>;
    pub async fn wait(&self) -> Result<Option<i32>, PtyError>;
    pub async fn kill(&self) -> Result<(), PtyError>;
}

pub fn spawn_command(cmd: &str, args: &[&str], opts: PtySpawnOptions) -> Result<PtyHandle, PtyError>;
pub fn spawn_harness(harness: HarnessId, opts: PtySpawnOptions) -> Result<PtyHandle, PtyError>;

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

/// Finalizes session worktree applying Squash, FastForward, Keep, or Discard.
pub async fn finish(
    worktree_path: &Path,
    strategy: MergeStrategy,
    base_branch: &str,
) -> Result<MergeOutcome, GitError>;
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
