use crate::quota::QuotaSnapshot;
use crate::types::{
    HarnessId, MergeStrategy, Mode, RouteOutcome, SessionId, SessionSummary, SessionTarget,
    TaskSize,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::PathBuf;

/// Current IPC protocol version.
pub const PROTOCOL_VERSION: u32 = 2;

/// Byte container that serializes to/from standard Base64 string in JSON.
///
/// Ensures PTY streams and scrollback buffers do NOT travel as JSON arrays of numbers.
#[derive(Clone, PartialEq, Eq)]
pub struct Base64Bytes(pub Vec<u8>);

impl Base64Bytes {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Base64Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Base64Bytes({} bytes)", self.0.len())
    }
}

impl Serialize for Base64Bytes {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = BASE64.encode(&self.0);
        serializer.serialize_str(&encoded)
    }
}

impl<'de> Deserialize<'de> for Base64Bytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let decoded = BASE64
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)?;
        Ok(Base64Bytes(decoded))
    }
}

impl From<Vec<u8>> for Base64Bytes {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<&[u8]> for Base64Bytes {
    fn from(s: &[u8]) -> Self {
        Self(s.to_vec())
    }
}

/// Messages sent from TUI Client (`aihub`) to Daemon (`aihubd`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "payload")]
pub enum ClientMessage {
    /// Handshake initiating connection with protocol version.
    Hello { version: u32 },
    /// Request the list of active/recent sessions.
    ListSessions,
    /// Create a new session worktree and launch specified harness.
    NewSession {
        harness: HarnessId,
        repo_path: PathBuf,
        initial_prompt: Option<String>,
    },
    /// Attach to an existing session or the latest session for a repository.
    Attach { target: SessionTarget },
    /// Detach client from active session without terminating it.
    Detach { session_id: SessionId },
    /// Send raw keystroke / input bytes to the session PTY.
    PtyInput {
        session_id: SessionId,
        data: Base64Bytes,
    },
    /// Resize the virtual terminal window.
    PtyResize {
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },
    /// Request an immediate refresh and push of quota snapshots.
    RequestQuota,
    /// Request task tier classification and harness/lane routing recommendation.
    RouteRequest {
        prompt: String,
        repo_path: Option<PathBuf>,
        size_hint: Option<TaskSize>,
    },
    /// Switch active session to a different agent harness, optionally generating a handoff brief.
    SwitchHarness {
        session_id: SessionId,
        target: HarnessId,
        with_handoff: bool,
        /// Explicit model id to pass to the incoming harness (blocker 5). Additive; absent on
        /// older clients defaults to `None`, which launches without a `--model` flag.
        #[serde(default)]
        model: Option<String>,
    },
    /// Toggle or set session operating mode (Assisted or Autonomous).
    SetMode { session_id: SessionId, mode: Mode },
    /// Conclude session and merge or discard worktree changes.
    MergeRequest {
        session_id: SessionId,
        strategy: MergeStrategy,
    },
    /// Submit task text for a session to provide routing context (F9).
    SubmitTask {
        session_id: SessionId,
        #[serde(alias = "prompt", alias = "text")]
        task: String,
    },
}

/// Messages sent from Daemon (`aihubd`) to TUI Client (`aihub`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "payload")]
pub enum DaemonMessage {
    /// Handshake response confirming protocol version.
    Hello { version: u32 },
    /// Response to `ListSessions`.
    SessionList { sessions: Vec<SessionSummary> },
    /// Confirmation that a new session worktree was created and launched.
    SessionCreated {
        session_id: SessionId,
        harness: HarnessId,
        worktree_path: PathBuf,
        branch: String,
    },
    /// Confirmation of attach, replaying terminal scrollback and session summary (F13).
    Attached {
        session_id: SessionId,
        scrollback: Base64Bytes,
        #[serde(default)]
        summary: SessionSummary,
    },
    /// Confirmation that client was detached.
    Detached { session_id: SessionId },
    /// Stream of output bytes from the running PTY.
    PtyOutput {
        session_id: SessionId,
        data: Base64Bytes,
    },
    /// Notification that the harness process terminated in the PTY.
    SessionExited {
        session_id: SessionId,
        exit_code: Option<i32>,
    },
    /// Pushed telemetry: current quota snapshots for all probed slots and lanes.
    QuotaPush { snapshots: Vec<QuotaSnapshot> },
    /// Routing recommendation reply tied to a session carrying a typed outcome (F10, F13).
    RouteRecommendation {
        session_id: SessionId,
        outcome: RouteOutcome,
    },
    /// Confirmation that harness switch succeeded.
    HarnessSwitched {
        session_id: SessionId,
        old_harness: HarnessId,
        new_harness: HarnessId,
        handoff_path: Option<PathBuf>,
    },
    /// Confirmation of mode update.
    ModeSet { session_id: SessionId, mode: Mode },
    /// Result of worktree merge or discard action.
    MergeResult {
        session_id: SessionId,
        success: bool,
        diff: String,
        message: String,
    },
    /// General or fatal IPC error.
    Error { code: String, message: String },
}

/// Top-level bidirectional IPC message envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "direction", content = "msg")]
pub enum IpcMessage {
    Client(ClientMessage),
    Daemon(DaemonMessage),
}

impl From<ClientMessage> for IpcMessage {
    fn from(c: ClientMessage) -> Self {
        IpcMessage::Client(c)
    }
}

impl From<DaemonMessage> for IpcMessage {
    fn from(d: DaemonMessage) -> Self {
        IpcMessage::Daemon(d)
    }
}
