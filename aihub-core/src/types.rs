use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Unique identifier for an aihub session.
///
/// Per system contract:
/// One aihub session = one git worktree = one branch `session/<session-id>`,
/// located at `${TMPDIR}/aihub/worktrees/<session-id>`.
/// A harness switch reuses the same session. The plan's separate "run-id"
/// folds into `SessionId`; there is no second id type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn branch_name(&self) -> String {
        format!("session/{}", self.0)
    }

    pub fn worktree_path(&self, worktree_root: &Path) -> PathBuf {
        worktree_root.join(&self.0)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Identifies supported AI coding agent harnesses.
///
/// Each harness knows its primary CLI binary name (`agy`, `claude`, `codex`, `cursor-agent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessId {
    ClaudeCode,
    Antigravity,
    Codex,
    CursorAgent,
}

impl HarnessId {
    /// Returns the primary binary name associated with this harness.
    pub fn binary_name(&self) -> &'static str {
        match self {
            HarnessId::ClaudeCode => "claude",
            HarnessId::Antigravity => "agy",
            HarnessId::Codex => "codex",
            HarnessId::CursorAgent => "cursor-agent",
        }
    }

    /// Resolves a harness from a binary name or common alias.
    pub fn from_binary_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "claude" | "claude-code" | "claudecode" => Some(HarnessId::ClaudeCode),
            "agy" | "antigravity" => Some(HarnessId::Antigravity),
            "codex" | "codex-cli" => Some(HarnessId::Codex),
            "cursor-agent" | "cursor" | "agent" => Some(HarnessId::CursorAgent),
            _ => None,
        }
    }

    /// All available harnesses.
    pub fn all() -> &'static [HarnessId] {
        &[
            HarnessId::ClaudeCode,
            HarnessId::Antigravity,
            HarnessId::Codex,
            HarnessId::CursorAgent,
        ]
    }
}

impl fmt::Display for HarnessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.binary_name())
    }
}

impl FromStr for HarnessId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_binary_name(s).ok_or_else(|| format!("unknown harness: {s}"))
    }
}

/// Classification tiers for incoming tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTier {
    /// Implementation, unit tests, linting, formatting, mechanical refactorings.
    Mechanical,
    /// Architecture, RFCs, high-level planning, open-ended decisions.
    Design,
    /// Code review, auditing, diff inspection, read-only analysis.
    Review,
}

impl fmt::Display for TaskTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskTier::Mechanical => write!(f, "mechanical"),
            TaskTier::Design => write!(f, "design"),
            TaskTier::Review => write!(f, "review"),
        }
    }
}

impl FromStr for TaskTier {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "mechanical" => Ok(TaskTier::Mechanical),
            "design" => Ok(TaskTier::Design),
            "review" => Ok(TaskTier::Review),
            _ => Err(format!("unknown task tier: {s}")),
        }
    }
}

/// Estimated task size / blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskSize {
    /// Small: 1-2 files.
    S,
    /// Medium: a single module / feature.
    M,
    /// Large: an entire subsystem or cross-cutting architectural change.
    L,
}

impl fmt::Display for TaskSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskSize::S => write!(f, "s"),
            TaskSize::M => write!(f, "m"),
            TaskSize::L => write!(f, "l"),
        }
    }
}

impl FromStr for TaskSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "s" | "small" => Ok(TaskSize::S),
            "m" | "medium" => Ok(TaskSize::M),
            "l" | "large" => Ok(TaskSize::L),
            _ => Err(format!("unknown task size: {s}")),
        }
    }
}

/// Operating mode of the interactive terminal session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Assisted: prompts user before switching harness when quota runs low or tier shifts.
    Assisted,
    /// Autonomous: switches harnesses and creates handoff briefs automatically.
    Autonomous,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mode::Assisted => write!(f, "assisted"),
            Mode::Autonomous => write!(f, "autonomous"),
        }
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "assisted" => Ok(Mode::Assisted),
            "autonomous" => Ok(Mode::Autonomous),
            _ => Err(format!("unknown mode: {s}")),
        }
    }
}

/// Git integration strategy when concluding a session worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MergeStrategy {
    /// Squash worktree commits onto the base branch.
    Squash,
    /// Fast-forward merge worktree onto base branch.
    FastForward,
    /// Keep worktree branch intact for manual review.
    Keep,
    /// Discard worktree branch cleanly without touching base repository.
    Discard,
}

impl fmt::Display for MergeStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MergeStrategy::Squash => write!(f, "squash"),
            MergeStrategy::FastForward => write!(f, "fast-forward"),
            MergeStrategy::Keep => write!(f, "keep"),
            MergeStrategy::Discard => write!(f, "discard"),
        }
    }
}

impl FromStr for MergeStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "squash" => Ok(MergeStrategy::Squash),
            "fast-forward" | "fastforward" | "ff" => Ok(MergeStrategy::FastForward),
            "keep" => Ok(MergeStrategy::Keep),
            "discard" => Ok(MergeStrategy::Discard),
            _ => Err(format!("unknown merge strategy: {s}")),
        }
    }
}

/// Target selection for attaching to a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTarget {
    Id(SessionId),
    LatestForRepo(PathBuf),
}

/// Lightweight summary of an active or recent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub harness: HarnessId,
    pub mode: Mode,
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub active: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub lane: Option<String>,
}

impl Default for SessionSummary {
    fn default() -> Self {
        Self {
            session_id: SessionId::new(""),
            harness: HarnessId::ClaudeCode,
            mode: Mode::Assisted,
            repo_path: PathBuf::new(),
            worktree_path: PathBuf::new(),
            branch: String::new(),
            active: false,
            model: None,
            lane: None,
        }
    }
}

/// Typed routing outcome for task dispatch (F10).
///
/// Distinguishes between a dispatchable recommendation and a typed no-capacity outcome,
/// preventing any placeholder harness sentinel from being mistaken for a launch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RouteOutcome {
    Recommendation {
        harness: HarnessId,
        lane: Option<String>,
        model: Option<String>,
        holds_until_s: Option<u64>,
    },
    NoCapacity {
        reason: String,
    },
}

pub type RoutingOutcome = RouteOutcome;

impl RouteOutcome {
    pub fn recommendation(
        harness: HarnessId,
        lane: Option<String>,
        model: Option<String>,
        holds_until_s: Option<u64>,
    ) -> Self {
        Self::Recommendation {
            harness,
            lane,
            model,
            holds_until_s,
        }
    }

    pub fn no_capacity(reason: impl Into<String>) -> Self {
        Self::NoCapacity {
            reason: reason.into(),
        }
    }

    pub fn is_dispatchable(&self) -> bool {
        matches!(self, RouteOutcome::Recommendation { .. })
    }

    pub fn harness(&self) -> Option<HarnessId> {
        match self {
            RouteOutcome::Recommendation { harness, .. } => Some(*harness),
            RouteOutcome::NoCapacity { .. } => None,
        }
    }

    pub fn lane(&self) -> Option<&str> {
        match self {
            RouteOutcome::Recommendation { lane, .. } => lane.as_deref(),
            RouteOutcome::NoCapacity { .. } => None,
        }
    }

    pub fn model(&self) -> Option<&str> {
        match self {
            RouteOutcome::Recommendation { model, .. } => model.as_deref(),
            RouteOutcome::NoCapacity { .. } => None,
        }
    }

    pub fn holds_until_s(&self) -> Option<u64> {
        match self {
            RouteOutcome::Recommendation { holds_until_s, .. } => *holds_until_s,
            RouteOutcome::NoCapacity { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            RouteOutcome::Recommendation { .. } => None,
            RouteOutcome::NoCapacity { reason } => Some(reason),
        }
    }
}
