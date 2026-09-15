use std::fmt;
use serde::{Deserialize, Serialize};
use crate::types::HarnessId;

/// Identifier for a quota slot (harness × account).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SlotId {
    pub harness: HarnessId,
    pub account: String,
}

impl SlotId {
    pub fn new(harness: HarnessId, account: impl Into<String>) -> Self {
        Self {
            harness,
            account: account.into(),
        }
    }

    /// Default slot for a harness.
    pub fn default_for(harness: HarnessId) -> Self {
        Self::new(harness, "default")
    }

    /// Composite key string (`<binary>:<account>`).
    pub fn key(&self) -> String {
        format!("{}:{}", self.harness.binary_name(), self.account)
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.key())
    }
}

/// Known quota window kinds across supported providers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    /// 5-hour rolling window (Claude Code, Antigravity, Codex).
    FiveHour,
    /// 7-day rolling window (Claude Code, Antigravity, Codex).
    SevenDay,
    /// Monthly billing cycle window (Cursor).
    Cycle,
    /// Custom or provider-specific window.
    Custom(String),
}

impl fmt::Display for WindowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WindowKind::FiveHour => write!(f, "five_hour"),
            WindowKind::SevenDay => write!(f, "seven_day"),
            WindowKind::Cycle => write!(f, "cycle"),
            WindowKind::Custom(s) => write!(f, "{s}"),
        }
    }
}

/// A quota window tracking consumption.
///
/// # Percentage Semantics
/// IMPORTANT: `used_pct` represents **PERCENTAGE USED** (0.0 to 100.0), NOT remaining.
/// Porters porting from `handoff.mjs` (which deals in `remaining_pct`) MUST convert:
/// `used_pct = 100.0 - remaining_pct`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub kind: WindowKind,
    /// Percentage of quota consumed / spent (0.0 to 100.0).
    /// State: this is USED percentage, never remaining percentage.
    pub used_pct: f64,
    /// Time in seconds until this window resets, if known.
    pub resets_in_s: Option<u64>,
    /// Total duration of this window in seconds, if known (e.g. 18000 for 5h, 604800 for 7d).
    pub window_s: Option<u64>,
}

impl QuotaWindow {
    pub fn new(kind: WindowKind, used_pct: f64, resets_in_s: Option<u64>, window_s: Option<u64>) -> Self {
        Self {
            kind,
            used_pct: used_pct.clamp(0.0, 100.0),
            resets_in_s,
            window_s,
        }
    }

    /// Convenience: remaining percentage (100.0 - used_pct).
    pub fn remaining_pct(&self) -> f64 {
        (100.0 - self.used_pct).max(0.0)
    }
}

/// Lane classification for models drawing on distinct quota pools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneKind {
    /// Provider's own models (e.g. Cursor Models: Auto/Composer/Grok; Antigravity: Gemini).
    Own,
    /// Third-party / frontier models (e.g. Cursor Other Models: Claude/GPT; Antigravity: 3p).
    Frontier,
}

impl fmt::Display for LaneKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaneKind::Own => write!(f, "own"),
            LaneKind::Frontier => write!(f, "frontier"),
        }
    }
}

/// A model lane pool within a slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaLane {
    /// Provider's lane name (e.g. "cursor-models", "other-models", "gemini", "third-party").
    pub name: String,
    /// Classification for routing: Own vs Frontier.
    pub kind: LaneKind,
    /// Windows specific to this lane.
    pub windows: Vec<QuotaWindow>,
}

/// Health status of a slot based on remaining quota and horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    /// Plentiful quota (used_pct < threshold).
    Ok,
    /// Low quota (remaining quota below low threshold, but may reopen within horizon).
    Low,
    /// Completely depleted (100% used or rate limited).
    Empty,
    /// Could not be probed or reading unavailable.
    Unknown,
}

impl fmt::Display for QuotaStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuotaStatus::Ok => write!(f, "ok"),
            QuotaStatus::Low => write!(f, "low"),
            QuotaStatus::Empty => write!(f, "empty"),
            QuotaStatus::Unknown => write!(f, "unknown"),
        }
    }
}

/// Source from which the quota reading was acquired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    /// Direct vendor endpoint or running daemon RPC (e.g. Antigravity language server).
    Vendor,
    /// OAuth token usage endpoint (e.g. Anthropic, OpenAI, Cursor).
    OAuth,
    /// Local session transcripts (e.g. ccusage / log rollups).
    Transcript,
}

impl fmt::Display for QuotaSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuotaSource::Vendor => write!(f, "vendor"),
            QuotaSource::OAuth => write!(f, "oauth"),
            QuotaSource::Transcript => write!(f, "transcript"),
        }
    }
}

/// Point-in-time snapshot of quota for an agent slot.
///
/// # Security
/// Per project contract, `QuotaSnapshot` contains NO credentials, tokens, or passwords.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub slot: SlotId,
    pub status: QuotaStatus,
    pub source: QuotaSource,
    pub estimated: bool,
    pub note: Option<String>,
    /// Every active window for this slot (five_hour, seven_day, cycle, etc.).
    pub windows: Vec<QuotaWindow>,
    /// Optional lane-specific breakdowns (e.g. own vs frontier).
    pub lanes: Vec<QuotaLane>,
}

impl QuotaSnapshot {
    /// Returns the tightest / most binding window across `windows`.
    /// The tightest window is the one with highest `used_pct`.
    pub fn tightest_window(&self) -> Option<&QuotaWindow> {
        self.windows.iter().max_by(|a, b| {
            a.used_pct
                .partial_cmp(&b.used_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// Overall remaining percentage based on the tightest window.
    pub fn remaining_pct(&self) -> Option<f64> {
        self.tightest_window().map(|w| w.remaining_pct())
    }

    /// Checks if all windows or the tightest window are within safe bounds.
    pub fn is_available(&self) -> bool {
        matches!(self.status, QuotaStatus::Ok | QuotaStatus::Low | QuotaStatus::Unknown)
    }
}
