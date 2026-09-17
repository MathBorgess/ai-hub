//! ai-memory adapter for handoff persistence.
//!
//! Citations:
//! - Upstream source: `github.com/akitaonrails/ai-memory` at git tag `v2.2.2`.
//! - Protocol route and tool handler: `crates/ai-memory-mcp/src/server.rs` (`memory_handoff_begin`,
//!   lines 3483-3568) mounted via StreamableHttpService at `POST /mcp` in
//!   `crates/ai-memory-cli/src/commands/serve.rs` (line 1299).
//! - Workspace and project resolution for cwd: `crates/ai-memory-hooks/src/router.rs`
//!   (`resolve_project_ids_inner`, lines 1966-2090) defaulting workspace to `"default"`
//!   (`DEFAULT_WORKSPACE_NAME`) and project to passed identity.
//! - Upstream `NewHandoff` schema: `crates/ai-memory-core/src/handoff.rs` (`v2.2.2`).
//! - Server URL & token environment variables: `docs/install.md` (`AI_MEMORY_SERVER_URL`
//!   defaulting to `http://127.0.0.1:49374`, and `AI_MEMORY_AUTH_TOKEN`).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aihub_core::{paths::default_data_dir, HarnessId, SessionId};
use serde::{Deserialize, Serialize};

use crate::brief::parse_brief_context;
use crate::{BriefPair, HandoffDestination, MemoryError};

/// Default URL for the local ai-memory server per docs/install.md.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49374";

/// Bounded number of spooled handoffs to drain per successful delivery.
pub const MAX_DRAIN_PER_CALL: usize = 20;

/// Whole-delivery deadline (connect, write, read): 5 seconds (N1).
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum allowed response body size: 1 MiB (N1).
pub const MAX_RESPONSE_BODY_SIZE: usize = 1024 * 1024;

/// Maximum spool file size: 10 MiB (N5).
pub const MAX_SPOOL_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum records in spool: 10,000 (N5).
pub const MAX_SPOOL_RECORDS: usize = 10_000;

/// Current schema version for spooled audit records (R6).
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

/// Locally spooled handoff record carrying the immutable sanitized payload (N4, R6).
/// Never stores paths to re-read later.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpooledRecord {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub from_harness: String,
    pub to_harness: String,
    pub project: String,
    pub recorded_at: String,
    pub goal: String,
    pub decisions: Vec<String>,
    pub last_output_summary: String,
}

impl SpooledRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: impl Into<String>,
        from_harness: impl Into<String>,
        to_harness: impl Into<String>,
        project: impl Into<String>,
        recorded_at: impl Into<String>,
        goal: impl Into<String>,
        decisions: Vec<String>,
        last_output_summary: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            session_id: session_id.into(),
            from_harness: from_harness.into(),
            to_harness: to_harness.into(),
            project: project.into(),
            recorded_at: recorded_at.into(),
            goal: goal.into(),
            decisions,
            last_output_summary: last_output_summary.into(),
        }
    }

    /// Creates a self-contained record by reading the brief file once at creation time (N4).
    pub fn from_brief(
        session_id: &SessionId,
        from: HarnessId,
        to: HarnessId,
        brief: &BriefPair,
        project: &str,
    ) -> Result<Self, MemoryError> {
        let content = std::fs::read_to_string(&brief.brief_path)?;
        let parsed = parse_brief_context(&content);

        Ok(Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            session_id: session_id.as_str().to_string(),
            from_harness: harness_label(from).to_string(),
            to_harness: harness_label(to).to_string(),
            project: project.to_string(),
            recorded_at: time_now_rfc3339(),
            goal: parsed.goal,
            decisions: parsed.decisions,
            last_output_summary: parsed.last_output_summary,
        })
    }
}

/// Records handoff metadata and returns whether it reached the live backend or was spooled locally (§3.5).
pub async fn record_handoff_destination(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    project: &str,
) -> Result<HandoffDestination, MemoryError> {
    let server_url =
        std::env::var("AI_MEMORY_SERVER_URL").unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string());
    let auth_token = std::env::var("AI_MEMORY_AUTH_TOKEN").ok();
    let data_dir = std::env::var_os("AIHUB_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir);

    record_handoff_to(
        session_id,
        from,
        to,
        brief,
        project,
        &server_url,
        auth_token.as_deref(),
        &data_dir,
    )
    .await
}

/// Convenience alias returning true if delivered to the backend, false if spooled locally.
pub async fn record_handoff_delivered(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    project: &str,
) -> Result<bool, MemoryError> {
    let dest = record_handoff_destination(session_id, from, to, brief, project).await?;
    Ok(dest.is_delivered())
}

/// Internal implementation taking explicit parameters for testing.
#[allow(clippy::too_many_arguments)]
pub async fn record_handoff_to(
    session_id: &SessionId,
    from: HarnessId,
    to: HarnessId,
    brief: &BriefPair,
    project: &str,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
) -> Result<HandoffDestination, MemoryError> {
    let record = SpooledRecord::from_brief(session_id, from, to, brief, project)?;
    record_spooled_record_to(&record, server_url, auth_token, data_dir).await
}

/// Delivers a spooled record or spools it on offline/failure.
pub async fn record_spooled_record_to(
    record: &SpooledRecord,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
) -> Result<HandoffDestination, MemoryError> {
    let spool_path = data_dir.join("handoffs.jsonl");

    // Attempt direct delivery to backend via MCP POST /mcp
    match deliver_record(server_url, auth_token, record).await {
        Ok(()) => {
            // Success: drain previously spooled records and surface drain errors (N3)
            drain_spool(server_url, auth_token, &spool_path, MAX_DRAIN_PER_CALL).await?;
            Ok(HandoffDestination::Delivered)
        }
        Err(_) => {
            // Offline fallback: deadline expired, body over cap, unreachable, or 4xx/5xx/MCP error (N1, N2)
            append_spool_async(&spool_path, record).await?;
            Ok(HandoffDestination::Spooled)
        }
    }
}

#[allow(dead_code)]
pub(crate) fn append_to_spool_file(
    spool_path: &Path,
    record: &SpooledRecord,
) -> Result<(), MemoryError> {
    append_spool(spool_path, record)
}

pub(crate) async fn append_to_spool_file_async(
    spool_path: &Path,
    record: &SpooledRecord,
) -> Result<(), MemoryError> {
    append_spool_async(spool_path, record).await
}

fn format_summary_payload(record: &SpooledRecord) -> String {
    let mut parts = Vec::new();
    if !record.goal.is_empty() {
        parts.push(record.goal.clone());
    }
    if !record.last_output_summary.is_empty() {
        parts.push(format!("Summary: {}", record.last_output_summary));
    }
    parts.push(format!(
        "Handoff from {} to {} for session {}",
        record.from_harness, record.to_harness, record.session_id
    ));
    parts.join("\n\n")
}

async fn deliver_record(
    server_url: &str,
    auth_token: Option<&str>,
    record: &SpooledRecord,
) -> Result<(), ()> {
    let summary = format_summary_payload(record);

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "memory_handoff_begin",
            "arguments": {
                "workspace": "default",
                "project": record.project,
                "summary": summary,
                "open_questions": Vec::<String>::new(),
                "next_steps": record.decisions,
                "files_touched": Vec::<String>::new(),
                "cwd": null,
                "shared": true
            }
        }
    });

    let body = serde_json::to_vec(&payload).map_err(|_| ())?;
    post_mcp(server_url, auth_token, &body, &serde_json::json!(1)).await
}

#[derive(Debug, Deserialize)]
struct LegacySpooledRecord {
    session_id: String,
    from_harness: String,
    to_harness: String,
    #[serde(default)]
    brief_path: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    prompt_path: Option<String>,
    recorded_at: String,
    #[serde(default)]
    project: Option<String>,
}

impl LegacySpooledRecord {
    fn migrate(self) -> SpooledRecord {
        let (goal, decisions, last_output_summary) = if let Some(ref bp) = self.brief_path {
            if let Ok(content) = std::fs::read_to_string(bp) {
                let parsed = parse_brief_context(&content);
                (parsed.goal, parsed.decisions, parsed.last_output_summary)
            } else {
                (
                    format!(
                        "Handoff from {} to {} for session {}",
                        self.from_harness, self.to_harness, self.session_id
                    ),
                    Vec::new(),
                    String::new(),
                )
            }
        } else {
            (
                format!(
                    "Handoff from {} to {} for session {}",
                    self.from_harness, self.to_harness, self.session_id
                ),
                Vec::new(),
                String::new(),
            )
        };

        SpooledRecord {
            schema_version: CURRENT_SCHEMA_VERSION,
            session_id: self.session_id,
            from_harness: self.from_harness,
            to_harness: self.to_harness,
            project: self.project.unwrap_or_else(|| "default".to_string()),
            recorded_at: self.recorded_at,
            goal,
            decisions,
            last_output_summary,
        }
    }
}

enum ParseLineResult {
    Valid(SpooledRecord),
    Migrated(SpooledRecord),
    Corrupt(String),
}

fn parse_record_line(line: &str) -> ParseLineResult {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ParseLineResult::Corrupt(line.to_string());
    }

    if let Ok(rec) = serde_json::from_str::<SpooledRecord>(trimmed) {
        return ParseLineResult::Valid(rec);
    }

    if let Ok(legacy) = serde_json::from_str::<LegacySpooledRecord>(trimmed) {
        return ParseLineResult::Migrated(legacy.migrate());
    }

    ParseLineResult::Corrupt(line.to_string())
}

fn quarantine_corrupt_record(parent: &Path, corrupt_line: &str) {
    let qpath = parent.join("handoffs.quarantine.jsonl");
    eprintln!("aihub-memory: quarantined corrupt record line: {corrupt_line}");
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    if let Ok(mut f) = options.open(&qpath) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        let _ = writeln!(f, "{corrupt_line}");
        let _ = f.sync_all();
    }
}

fn recover_partial_tail(parent: &Path, file: &mut std::fs::File) -> Result<(), MemoryError> {
    use std::io::{Read, Seek, SeekFrom};
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }

    file.seek(SeekFrom::End(-1))?;
    let mut last_byte = [0u8; 1];
    file.read_exact(&mut last_byte)?;
    if last_byte[0] == b'\n' {
        return Ok(());
    }

    let mut pos = len;
    let mut found_nl = None;
    let mut buffer = [0u8; 1024];
    while pos > 0 {
        let chunk_size = std::cmp::min(pos, 1024);
        pos -= chunk_size;
        file.seek(SeekFrom::Start(pos))?;
        let n = file.read(&mut buffer[..chunk_size as usize])?;
        if let Some(nl_idx) = buffer[..n].iter().rposition(|&b| b == b'\n') {
            found_nl = Some(pos + nl_idx as u64);
            break;
        }
    }

    let cut_pos = match found_nl {
        Some(p) => p + 1,
        None => 0,
    };

    let partial_len = (len - cut_pos) as usize;
    file.seek(SeekFrom::Start(cut_pos))?;
    let mut partial_bytes = vec![0u8; partial_len];
    file.read_exact(&mut partial_bytes)?;

    let partial_str = String::from_utf8_lossy(&partial_bytes);
    quarantine_corrupt_record(parent, &format!("[partial-write-tail] {partial_str}"));

    file.set_len(cut_pos)?;
    file.seek(SeekFrom::End(0))?;
    file.sync_all()?;
    Ok(())
}

/// Ensures directory exists with 0700 permissions (N3, Permissions, R7).
pub(crate) fn ensure_spool_dir(dir: &Path) -> Result<(), MemoryError> {
    ensure_spool_dir_with_seam(dir, None)
}

pub(crate) type ChmodHook<'a> = &'a (dyn Fn(&Path) -> std::io::Result<()> + Send + Sync);

pub(crate) fn ensure_spool_dir_with_seam(
    dir: &Path,
    chmod_seam: Option<ChmodHook>,
) -> Result<(), MemoryError> {
    if dir.as_os_str().is_empty() {
        return Ok(());
    }

    if let Ok(meta) = std::fs::symlink_metadata(dir) {
        if meta.file_type().is_symlink() {
            return Err(MemoryError::UnsafePermissions(format!(
                "spool directory cannot be a symlink: {}",
                dir.display()
            )));
        }
        if !meta.is_dir() {
            return Err(MemoryError::UnsafePermissions(format!(
                "spool path is not a directory: {}",
                dir.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let current_uid = unsafe { libc::getuid() };
            if meta.uid() != current_uid {
                return Err(MemoryError::UnsafePermissions(format!(
                    "spool directory {} owned by uid {} instead of current uid {}",
                    dir.display(),
                    meta.uid(),
                    current_uid
                )));
            }
        }
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(dir)?;
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(dir)?;
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(seam) = chmod_seam {
            seam(dir)?;
        } else {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let meta = std::fs::metadata(dir)?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(MemoryError::UnsafePermissions(format!(
                "directory {} has mode {:o}, expected 0700",
                dir.display(),
                mode
            )));
        }
    }

    Ok(())
}

fn open_secure_file(
    path: &Path,
    read: bool,
    write: bool,
    create: bool,
    append: bool,
) -> Result<std::fs::File, MemoryError> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            return Err(MemoryError::UnsafePermissions(format!(
                "refusing symlinked target: {}",
                path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let current_uid = unsafe { libc::getuid() };
            if meta.uid() != current_uid {
                return Err(MemoryError::UnsafePermissions(format!(
                    "file {} owned by uid {} instead of current uid {}",
                    path.display(),
                    meta.uid(),
                    current_uid
                )));
            }
        }
    }

    let mut options = OpenOptions::new();
    options
        .read(read)
        .write(write)
        .create(create)
        .append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }

    let file = options.open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(MemoryError::UnsafePermissions(format!(
                "not a regular file: {}",
                path.display()
            )));
        }
        let current_uid = unsafe { libc::getuid() };
        if meta.uid() != current_uid {
            return Err(MemoryError::UnsafePermissions(format!(
                "opened file {} owned by uid {} instead of current uid {}",
                path.display(),
                meta.uid(),
                current_uid
            )));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let updated_meta = file.metadata()?;
        let mode = updated_meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(MemoryError::UnsafePermissions(format!(
                "file {} mode is {:o}, expected 0600",
                path.display(),
                mode
            )));
        }
    }

    Ok(file)
}

/// In-process async mutex to serialize drains without blocking executor threads (R4).
static DRAIN_ASYNC_MUTEX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Cross-process exclusive file lock for spool access (N3, R4).
pub(crate) struct SpoolLock {
    file: std::fs::File,
}

impl SpoolLock {
    pub(crate) fn acquire(lock_path: &Path) -> Result<Self, MemoryError> {
        Self::acquire_timeout(lock_path, Duration::from_secs(5))
    }

    pub(crate) fn acquire_timeout(
        lock_path: &Path,
        timeout: Duration,
    ) -> Result<Self, MemoryError> {
        if let Some(parent) = lock_path.parent() {
            ensure_spool_dir(parent)?;
        }
        let file = open_secure_file(lock_path, true, true, true, false)?;

        let start = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(e) => {
                    if start.elapsed() >= timeout {
                        return Err(MemoryError::LockError(format!(
                            "timed out after {:?} acquiring spool lock at {}: {e}",
                            timeout,
                            lock_path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    pub(crate) async fn acquire_async(
        lock_path: &Path,
        timeout: Duration,
    ) -> Result<Self, MemoryError> {
        let path = lock_path.to_path_buf();
        tokio::task::spawn_blocking(move || Self::acquire_timeout(&path, timeout))
            .await
            .map_err(|e| MemoryError::LockError(format!("spawn_blocking failed: {e}")))?
    }
}

impl Drop for SpoolLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn append_spool(spool_path: &Path, record: &SpooledRecord) -> Result<(), MemoryError> {
    let parent = spool_path.parent().unwrap_or_else(|| Path::new("."));
    ensure_spool_dir(parent)?;

    let lock_path = parent.join("handoffs.lock");
    let _lock = SpoolLock::acquire(&lock_path)?;

    let line = serde_json::to_string(record)?;
    let line_bytes = line.len() as u64 + 1; // +1 for newline

    // Check single-record size limit unconditionally, even when absent (R7)
    if line_bytes > MAX_SPOOL_BYTES {
        return Err(MemoryError::SpoolFull(format!(
            "record size ({line_bytes} bytes) exceeds maximum spool capacity ({MAX_SPOOL_BYTES} bytes)"
        )));
    }

    if spool_path.exists() {
        let meta = std::fs::symlink_metadata(spool_path)?;
        let current_size = meta.len();
        if current_size + line_bytes > MAX_SPOOL_BYTES {
            return Err(MemoryError::SpoolFull(format!(
                "spool size limit (10 MiB) reached: current {current_size} bytes, adding {line_bytes} bytes"
            )));
        }

        // Count records by streaming lines
        let file = open_secure_file(spool_path, true, false, false, false)?;
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(file);
        let mut count = 0usize;
        for l in reader.lines() {
            let l = l?;
            if !l.trim().is_empty() {
                count += 1;
            }
        }
        if count >= MAX_SPOOL_RECORDS {
            return Err(MemoryError::SpoolFull(format!(
                "spool record count limit (10,000) reached: {count} records"
            )));
        }
    }

    let mut file = open_secure_file(spool_path, true, true, true, true)?;

    // Recover partial tail if previous write was interrupted (R6)
    recover_partial_tail(parent, &mut file)?;

    writeln!(file, "{line}")?;
    file.sync_all()?;

    // Sync parent directory when spool file is created or updated (R6)
    if let Ok(dir_file) = std::fs::File::open(parent) {
        let _ = dir_file.sync_all();
    }

    Ok(())
}

pub(crate) async fn append_spool_async(
    spool_path: &Path,
    record: &SpooledRecord,
) -> Result<(), MemoryError> {
    let path = spool_path.to_path_buf();
    let rec = record.clone();
    tokio::task::spawn_blocking(move || append_spool(&path, &rec))
        .await
        .map_err(|e| MemoryError::LockError(format!("spawn_blocking failed: {e}")))?
}

/// Drains oldest spooled records first, bounded by `limit`.
pub async fn drain_spool(
    server_url: &str,
    auth_token: Option<&str>,
    spool_path: &Path,
    limit: usize,
) -> Result<usize, MemoryError> {
    drain_spool_with_hook(server_url, auth_token, spool_path, limit, None).await
}

/// Drains spool with an optional hook called before renaming remainder (narrow seam for N3 testing).
pub async fn drain_spool_with_hook(
    server_url: &str,
    auth_token: Option<&str>,
    spool_path: &Path,
    limit: usize,
    before_rename: Option<&(dyn Fn() -> Result<(), MemoryError> + Send + Sync)>,
) -> Result<usize, MemoryError> {
    let parent = spool_path.parent().unwrap_or_else(|| Path::new("."));
    ensure_spool_dir(parent)?;

    // Serialize drains asynchronously in-process (R4)
    let _drain_guard = DRAIN_ASYNC_MUTEX.lock().await;

    let lock_path = parent.join("handoffs.lock");

    // 1. Acquire lock off-executor (bounded) to inspect spool and recover any partial tail (R4, R6)
    let candidate_records = {
        let _lock = SpoolLock::acquire_async(&lock_path, Duration::from_secs(5)).await?;

        if !spool_path.exists() {
            return Ok(0);
        }

        {
            let mut file = open_secure_file(spool_path, true, true, false, false)?;
            recover_partial_tail(parent, &mut file)?;
        }

        let file = open_secure_file(spool_path, true, false, false, false)?;
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(file);

        let mut candidates = Vec::new();
        for line_res in reader.lines() {
            let line = line_res?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match parse_record_line(trimmed) {
                ParseLineResult::Valid(rec) | ParseLineResult::Migrated(rec) => {
                    if candidates.len() < limit {
                        candidates.push(rec);
                    }
                }
                ParseLineResult::Corrupt(bad) => {
                    quarantine_corrupt_record(parent, &bad);
                }
            }
        }
        candidates
        // Lock is DROPPED here before network await (R4)!
    };

    if candidate_records.is_empty() {
        return Ok(0);
    }

    // 2. Deliver records over network WITHOUT holding the file lock (R4)!
    // Simultaneous appends from other sessions / threads can proceed without being blocked!
    let mut delivered_records = Vec::new();
    for rec in &candidate_records {
        if deliver_record(server_url, auth_token, rec).await.is_ok() {
            delivered_records.push(rec.clone());
        } else {
            break;
        }
    }

    if delivered_records.is_empty() {
        return Ok(0);
    }

    let drained_count = delivered_records.len();

    // 3. Re-acquire lock off-executor (bounded) to atomically rewrite remainder (R4, N3)
    {
        let _lock = SpoolLock::acquire_async(&lock_path, Duration::from_secs(5)).await?;

        if !spool_path.exists() {
            return Ok(drained_count);
        }

        let file = open_secure_file(spool_path, true, false, false, false)?;
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(file);

        let mut remaining_records: Vec<String> = Vec::new();
        let mut to_remove = delivered_records;

        for line_res in reader.lines() {
            let line = line_res?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match parse_record_line(trimmed) {
                ParseLineResult::Valid(rec) | ParseLineResult::Migrated(rec) => {
                    if let Some(pos) = to_remove.iter().position(|d| {
                        d.session_id == rec.session_id && d.recorded_at == rec.recorded_at
                    }) {
                        to_remove.remove(pos);
                    } else {
                        remaining_records.push(line);
                    }
                }
                ParseLineResult::Corrupt(_) => {
                    // Already quarantined; omit from remainder
                }
            }
        }

        let temp_file_name = format!(
            ".handoffs.jsonl.tmp.{}.{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let temp_path = parent.join(temp_file_name);

        let write_res = (|| -> Result<(), MemoryError> {
            let mut temp_file = open_secure_file(&temp_path, false, true, true, false)?;
            for rem in &remaining_records {
                writeln!(temp_file, "{rem}")?;
            }
            temp_file.sync_all()?;

            if let Some(hook) = before_rename {
                hook()?;
            }

            std::fs::rename(&temp_path, spool_path)?;

            let dir_file = std::fs::File::open(parent)?;
            dir_file.sync_all()?;
            Ok(())
        })();

        if let Err(e) = write_res {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e);
        }
    }

    Ok(drained_count)
}

fn harness_label(h: HarnessId) -> &'static str {
    match h {
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Codex => "codex",
        HarnessId::CursorAgent => "cursor-agent",
        HarnessId::Antigravity => "antigravity",
    }
}

fn time_now_rfc3339() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}Z", dur.as_secs())
}

fn parse_url(raw: &str) -> Result<(String, u16), std::io::Error> {
    let s = raw.strip_prefix("http://").unwrap_or(raw);
    let host_port = s.split('/').next().unwrap_or(s);
    let (host, port_str) = host_port.split_once(':').unwrap_or((host_port, "49374"));
    let port: u16 = port_str
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    Ok((host.to_string(), port))
}

const MAX_HEADER_SIZE: usize = 64 * 1024;

async fn post_mcp(
    url: &str,
    auth_token: Option<&str>,
    body: &[u8],
    expected_id: &serde_json::Value,
) -> Result<(), ()> {
    tokio::time::timeout(
        DELIVERY_TIMEOUT,
        post_mcp_raw(url, auth_token, body, expected_id),
    )
    .await
    .map_err(|_| ())?
}

struct ChunkDecoder {
    buf: Vec<u8>,
    decoded: Vec<u8>,
    finished: bool,
}

impl ChunkDecoder {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            decoded: Vec::new(),
            finished: false,
        }
    }

    fn feed(&mut self, data: &[u8]) -> Result<(Vec<u8>, bool), ()> {
        self.buf.extend_from_slice(data);
        let mut newly_decoded = Vec::new();

        loop {
            if self.finished {
                break;
            }

            let crlf_pos = match self.buf.windows(2).position(|w| w == b"\r\n") {
                Some(pos) => pos,
                None => break,
            };

            let header_str = std::str::from_utf8(&self.buf[..crlf_pos]).map_err(|_| ())?;
            let size_str = header_str.split(';').next().unwrap_or("").trim();
            let chunk_size = usize::from_str_radix(size_str, 16).map_err(|_| ())?;

            if chunk_size == 0 {
                let trailer_end = match self.buf[crlf_pos + 2..]
                    .windows(2)
                    .position(|w| w == b"\r\n")
                {
                    Some(pos) => crlf_pos + 2 + pos + 2,
                    None => break,
                };
                self.buf.drain(..trailer_end);
                self.finished = true;
                break;
            }

            let chunk_data_start = crlf_pos + 2;
            let chunk_data_end = chunk_data_start + chunk_size;
            let total_chunk_end = chunk_data_end + 2;

            if self.buf.len() < total_chunk_end {
                break;
            }

            let chunk_data = &self.buf[chunk_data_start..chunk_data_end];
            newly_decoded.extend_from_slice(chunk_data);
            self.decoded.extend_from_slice(chunk_data);
            if self.decoded.len() > MAX_RESPONSE_BODY_SIZE {
                return Err(());
            }

            self.buf.drain(..total_chunk_end);
        }

        Ok((newly_decoded, self.finished))
    }

    fn into_bytes(self) -> Result<Vec<u8>, ()> {
        if !self.finished {
            return Err(());
        }
        Ok(self.decoded)
    }
}

struct SseParser {
    buf: Vec<u8>,
    total_bytes: usize,
}

impl SseParser {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            total_bytes: 0,
        }
    }

    fn feed(
        &mut self,
        chunk: &[u8],
        expected_id: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, ()> {
        self.buf.extend_from_slice(chunk);
        self.total_bytes += chunk.len();
        if self.total_bytes > MAX_RESPONSE_BODY_SIZE {
            return Err(());
        }

        loop {
            let (event_len, delim_len) =
                if let Some(pos) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    (pos, 4)
                } else if let Some(pos) = self.buf.windows(2).position(|w| w == b"\n\n") {
                    (pos, 2)
                } else {
                    break;
                };

            let event_bytes = self.buf[..event_len].to_vec();
            self.buf.drain(..event_len + delim_len);

            let event_str = match std::str::from_utf8(&event_bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let mut data_lines = Vec::new();
            for line in event_str.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("data:") {
                    let d = rest.trim();
                    if !d.is_empty() {
                        data_lines.push(d);
                    }
                }
            }

            if data_lines.is_empty() {
                continue;
            }

            let joined_data = data_lines.join("\n");
            let json_val: serde_json::Value = match serde_json::from_str(&joined_data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if !json_val.is_object() {
                continue;
            }

            if json_val.get("id") == Some(expected_id) {
                if let Some(err) = json_val.get("error") {
                    if !err.is_null() {
                        return Err(());
                    }
                }

                let result = json_val.get("result").ok_or(())?;
                if !result.is_object() {
                    return Err(());
                }

                if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
                    return Err(());
                }

                return Ok(Some(json_val));
            }
        }

        Ok(None)
    }
}

#[derive(PartialEq, Eq)]
enum ProcessResult {
    Continue,
    Finished,
}

#[allow(clippy::too_many_arguments)]
fn process_body_chunk(
    chunk: &[u8],
    is_chunked: bool,
    is_sse: bool,
    content_length: Option<usize>,
    body_accum: &mut Vec<u8>,
    chunk_decoder: &mut ChunkDecoder,
    sse_parser: &mut SseParser,
    expected_id: &serde_json::Value,
) -> Result<ProcessResult, ()> {
    if is_chunked {
        let (newly_decoded, finished) = chunk_decoder.feed(chunk)?;
        if is_sse {
            if sse_parser.feed(&newly_decoded, expected_id)?.is_some() {
                return Ok(ProcessResult::Finished);
            }
        } else {
            body_accum.extend_from_slice(&newly_decoded);
            if body_accum.len() > MAX_RESPONSE_BODY_SIZE {
                return Err(());
            }
            if finished {
                return Ok(ProcessResult::Finished);
            }
        }
    } else if is_sse {
        if sse_parser.feed(chunk, expected_id)?.is_some() {
            return Ok(ProcessResult::Finished);
        }
    } else {
        body_accum.extend_from_slice(chunk);
        if body_accum.len() > MAX_RESPONSE_BODY_SIZE {
            return Err(());
        }
        if let Some(cl) = content_length {
            if body_accum.len() >= cl {
                return Ok(ProcessResult::Finished);
            }
        }
    }

    Ok(ProcessResult::Continue)
}

async fn post_mcp_raw(
    url: &str,
    auth_token: Option<&str>,
    body: &[u8],
    expected_id: &serde_json::Value,
) -> Result<(), ()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (host, port) = parse_url(url).map_err(|_| ())?;
    let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|_| ())?;

    let auth_header = match auth_token {
        Some(tok) if !tok.is_empty() => format!("Authorization: Bearer {tok}\r\n"),
        _ => String::new(),
    };

    let req = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Accept: application/json, text/event-stream\r\n\
         {auth_header}\
         Connection: close\r\n\
         \r\n",
        body.len()
    );

    stream.write_all(req.as_bytes()).await.map_err(|_| ())?;
    stream.write_all(body).await.map_err(|_| ())?;
    stream.flush().await.map_err(|_| ())?;

    let mut resp_bytes = Vec::new();
    let mut buf = [0u8; 8192];
    let mut header_end = None;
    let mut status_code = None;
    let mut is_chunked = false;
    let mut is_sse = false;
    let mut content_length: Option<usize> = None;

    let mut body_accum = Vec::new();
    let mut chunk_decoder = ChunkDecoder::new();
    let mut sse_parser = SseParser::new();

    loop {
        let n = stream.read(&mut buf).await.map_err(|_| ())?;
        if n == 0 {
            break;
        }

        if header_end.is_none() {
            resp_bytes.extend_from_slice(&buf[..n]);
            if resp_bytes.len() > MAX_HEADER_SIZE {
                return Err(());
            }

            if let Some(pos) = resp_bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let end = pos + 4;
                header_end = Some(end);

                let header_str = std::str::from_utf8(&resp_bytes[..pos]).map_err(|_| ())?;
                let code = parse_http_status(&resp_bytes[..end]).ok_or(())?;
                if !(200..300).contains(&code) {
                    return Err(());
                }
                status_code = Some(code);

                for line in header_str.lines() {
                    let line_lower = line.to_ascii_lowercase();
                    if let Some(val) = line_lower.strip_prefix("content-length:") {
                        let len = val.trim().parse::<usize>().map_err(|_| ())?;
                        if len > MAX_RESPONSE_BODY_SIZE {
                            return Err(());
                        }
                        content_length = Some(len);
                    } else if let Some(val) = line_lower.strip_prefix("transfer-encoding:") {
                        if val.contains("chunked") {
                            is_chunked = true;
                        }
                    } else if let Some(val) = line_lower.strip_prefix("content-type:") {
                        if val.contains("text/event-stream") {
                            is_sse = true;
                        }
                    }
                }

                let remaining_body = &resp_bytes[end..];
                if !remaining_body.is_empty() {
                    let res = process_body_chunk(
                        remaining_body,
                        is_chunked,
                        is_sse,
                        content_length,
                        &mut body_accum,
                        &mut chunk_decoder,
                        &mut sse_parser,
                        expected_id,
                    )?;
                    if res == ProcessResult::Finished {
                        if is_sse {
                            return Ok(());
                        } else {
                            break;
                        }
                    }
                }
            }
        } else {
            let res = process_body_chunk(
                &buf[..n],
                is_chunked,
                is_sse,
                content_length,
                &mut body_accum,
                &mut chunk_decoder,
                &mut sse_parser,
                expected_id,
            )?;
            if res == ProcessResult::Finished {
                if is_sse {
                    return Ok(());
                } else {
                    break;
                }
            }
        }
    }

    let code = status_code.ok_or(())?;

    let final_body = if is_chunked {
        chunk_decoder.into_bytes()?
    } else {
        body_accum
    };

    validate_mcp_response(code, &final_body, expected_id)
}

fn parse_http_status(resp: &[u8]) -> Option<u16> {
    let header_text = std::str::from_utf8(resp).ok()?;
    let first_line = header_text.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _proto = parts.next()?;
    let code_str = parts.next()?;
    code_str.parse::<u16>().ok()
}

/// Validates MCP delivery response per N2, R5:
/// 2xx status, valid JSON or SSE framing, id match, no top-level error, result without isError: true.
pub(crate) fn validate_mcp_response(
    status: u16,
    body: &[u8],
    expected_id: &serde_json::Value,
) -> Result<(), ()> {
    if !(200..300).contains(&status) {
        return Err(());
    }
    if body.is_empty() {
        return Err(());
    }

    let json_val = parse_json_or_sse(body, expected_id)?;

    if json_val.get("id") != Some(expected_id) {
        return Err(());
    }

    if let Some(err) = json_val.get("error") {
        if !err.is_null() {
            return Err(());
        }
    }

    let result = json_val.get("result").ok_or(())?;
    if !result.is_object() {
        return Err(());
    }

    if let Some(is_error) = result.get("isError").and_then(|v| v.as_bool()) {
        if is_error {
            return Err(());
        }
    }

    Ok(())
}

fn parse_json_or_sse(
    body: &[u8],
    expected_id: &serde_json::Value,
) -> Result<serde_json::Value, ()> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if v.is_object()
            && (v.get("jsonrpc").is_some() || v.get("result").is_some() || v.get("error").is_some())
        {
            return Ok(v);
        }
    }

    let mut parser = SseParser::new();
    if let Some(v) = parser.feed(body, expected_id)? {
        return Ok(v);
    }

    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    fn offline_server_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    async fn start_fake_server(
        status_code: u16,
        return_body: &'static str,
        content_type: &'static str,
        max_accepts: usize,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let req_clone = requests.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let _ = ready_tx.send(());
            for _ in 0..max_accepts {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(pos + 4);
                    }
                };
                if let Some(header_end) = header_end {
                    let content_length = String::from_utf8_lossy(&buf[..header_end])
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().to_string())
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    while buf.len() < header_end + content_length {
                        let mut chunk = [0u8; 4096];
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                }
                let req_str = String::from_utf8_lossy(&buf).to_string();
                req_clone.lock().await.push(req_str);

                let resp = format!(
                    "HTTP/1.1 {} Status\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_code,
                    content_type,
                    return_body.len(),
                    return_body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        ready_rx
            .await
            .expect("fake ai-memory server task ended early");
        (port, requests)
    }

    #[tokio::test]
    async fn test_adapter_success_delivers_to_ai_memory() {
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-123"}}"#,
            "application/json",
            3,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-success-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let worktree = dir.join("my-cool-project");
        let handoffs_dir = worktree.join(".aihub").join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nShip ai-memory adapter\n## Constraints\n- Context: initial\n- Decision: use rust\n## Last turn (outgoing harness)\nDone shipping\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-success");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "my-cool-project",
            &server_url,
            Some("fake-token"),
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Delivered);
        assert!(dest.is_delivered());
        assert!(!dest.is_spooled());

        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].contains("POST /mcp HTTP/1.1"));
        assert!(recorded[0].contains("Authorization: Bearer fake-token"));
        assert!(recorded[0].contains("\"memory_handoff_begin\""));
        assert!(recorded[0].contains("\"project\":\"my-cool-project\""));
        assert!(recorded[0].contains("\"workspace\":\"default\""));
        assert!(recorded[0].contains("Decision: use rust"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_adapter_401_unauthorized_spools_locally() {
        let (port, _) = start_fake_server(401, "", "application/json", 1).await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-401-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest 401 fallback\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-401");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj401",
            &server_url,
            Some("bad-token"),
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let content = std::fs::read_to_string(spool_file).unwrap();
        assert!(content.contains("sess-401"));
        assert!(content.contains("proj401"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_adapter_unreachable_spools_locally() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-unreach-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest unreachable\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-unreach");
        let server_url = offline_server_url();

        let dest = record_handoff_to(
            &session_id,
            HarnessId::CursorAgent,
            HarnessId::Antigravity,
            &brief,
            "projunreach",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let content = std::fs::read_to_string(spool_file).unwrap();
        assert!(content.contains("sess-unreach"));
        assert!(content.contains("projunreach"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_adapter_drain_on_later_success() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-drain-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief1 = handoffs_dir.join("01.md");
        std::fs::write(&brief1, "## Goal\nFirst spooled handoff\n").unwrap();
        let prompt1 = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt1, "p1").unwrap();

        let brief2 = handoffs_dir.join("02.md");
        std::fs::write(&brief2, "## Goal\nSecond live handoff\n").unwrap();
        let prompt2 = handoffs_dir.join("02.prompt.md");
        std::fs::write(&prompt2, "p2").unwrap();

        let sid1 = SessionId::new("sess-spooled-1");
        let sid2 = SessionId::new("sess-live-2");

        // 1. Spool first handoff while offline
        let offline_url = offline_server_url();
        let dest1 = record_handoff_to(
            &sid1,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &BriefPair {
                brief_path: brief1,
                prompt_path: prompt1,
            },
            "projdrain",
            &offline_url,
            None,
            &dir,
        )
        .await
        .unwrap();
        assert_eq!(dest1, HandoffDestination::Spooled);

        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let spool_content = std::fs::read_to_string(&spool_file).unwrap();
        assert!(spool_content.contains("sess-spooled-1"));

        // 2. Start fake server
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"ok"}}"#,
            "application/json",
            3,
        )
        .await;
        let server_url = format!("http://127.0.0.1:{port}");

        // 3. Record second handoff successfully -> drains spool!
        let dest2 = record_handoff_to(
            &sid2,
            HarnessId::Codex,
            HarnessId::Antigravity,
            &BriefPair {
                brief_path: brief2,
                prompt_path: prompt2,
            },
            "projdrain",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest2, HandoffDestination::Delivered);

        let mut len = 0usize;
        for _ in 0..50 {
            len = reqs.lock().await.len();
            if len >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(len, 2);
        let recorded = reqs.lock().await;
        let has_live = recorded.iter().any(|r| r.contains("Second live handoff"));
        let has_drained = recorded.iter().any(|r| r.contains("First spooled handoff"));
        assert!(has_live);
        assert!(has_drained);

        if spool_file.exists() {
            let rem = std::fs::read_to_string(&spool_file).unwrap();
            assert!(rem.trim().is_empty());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Regression Findings Tests ---

    #[tokio::test]
    async fn n1_silent_server_spools_within_deadline() {
        // Silent server: accepts TCP connection but never sends any bytes back
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            let _ = ready_tx.send(());
            if let Ok((mut socket, _)) = listener.accept().await {
                // Read request but never respond, keep open until timeout
                let mut buf = [0u8; 1024];
                while socket.read(&mut buf).await.unwrap_or(0) > 0 {}
            }
        });
        ready_rx.await.unwrap();

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n1-silent-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nSilent server test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let start = tokio::time::Instant::now();
        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n1-silent"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n1",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        let elapsed = start.elapsed();
        // Deadline is 5 seconds. Must complete within ~6 seconds and spool!
        assert!(elapsed >= Duration::from_secs(4));
        assert!(elapsed < Duration::from_secs(7));
        assert_eq!(dest, HandoffDestination::Spooled);

        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let content = std::fs::read_to_string(spool_file).unwrap();
        assert!(content.contains("sess-n1-silent"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n1_response_body_over_cap_spools_record() {
        // Server sends response body over 1 MiB
        let oversized_body = "x".repeat(MAX_RESPONSE_BODY_SIZE + 1024);
        let (port, _) = start_fake_server(
            200,
            Box::leak(oversized_body.into_boxed_str()),
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n1-cap-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nBody over cap test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n1-cap"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n1",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n2_tool_is_error_true_spools_record() {
        // 200 with result.isError = true
        let (port, _) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"failed"}]}}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n2-iserror-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTool isError test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n2-iserror"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n2_empty_body_and_malformed_spools_record() {
        // 200 with empty body
        let (port, _) = start_fake_server(200, "", "application/json", 1).await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n2-empty-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nEmpty body test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n2-empty"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n2_sse_error_spools_and_sse_success_delivers() {
        // 1. SSE carrying RPC error
        let (port_err, _) = start_fake_server(
            200,
            "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32603,\"message\":\"internal\"}}\r\n\r\n",
            "text/event-stream",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n2-sse-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nSSE test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port_err}");
        let dest_err = record_handoff_to(
            &SessionId::new("sess-n2-sse-err"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest_err, HandoffDestination::Spooled);

        // 2. SSE carrying valid result
        let (port_ok, _) = start_fake_server(
            200,
            "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"handoff_id\":\"h-sse\",\"isError\":false}}\r\n\r\n",
            "text/event-stream",
            2,
        )
        .await;
        let server_url_ok = format!("http://127.0.0.1:{port_ok}");
        let dest_ok = record_handoff_to(
            &SessionId::new("sess-n2-sse-ok"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url_ok,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest_ok, HandoffDestination::Delivered);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n2_mismatched_id_spools_record() {
        // Server responds with mismatched ID 999 instead of 1
        let (port, _) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":999,"result":{"handoff_id":"wrong-id"}}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n2-id-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nID mismatch test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n2-id"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-n2",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Spooled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n3_interrupted_rewrite_keeps_pending_records() {
        let (port, _) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"ok"}}"#,
            "application/json",
            5,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n3-rewrite-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let spool_path = dir.join("handoffs.jsonl");

        let rec1 = SpooledRecord::new(
            "s1",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g1",
            vec![],
            "sum1",
        );
        let rec2 = SpooledRecord::new(
            "s2",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g2",
            vec![],
            "sum2",
        );
        append_spool(&spool_path, &rec1).unwrap();
        append_spool(&spool_path, &rec2).unwrap();

        let initial_spool = std::fs::read_to_string(&spool_path).unwrap();

        // Inject failure between temp file write and rename
        let server_url = format!("http://127.0.0.1:{port}");
        let hook = || -> Result<(), MemoryError> {
            Err(MemoryError::Io(std::io::Error::other(
                "injected disk-full crash before rename",
            )))
        };

        let res = drain_spool_with_hook(&server_url, None, &spool_path, 1, Some(&hook)).await;
        assert!(res.is_err(), "interrupted rewrite must return error");

        // The original spool MUST be completely intact!
        let current_spool = std::fs::read_to_string(&spool_path).unwrap();
        assert_eq!(
            current_spool, initial_spool,
            "original spool must be intact after crash before rename"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n3_cross_process_lock_serializes_append_and_drain() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n3-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let lock_path = dir.join("handoffs.lock");

        let lock1 = SpoolLock::acquire(&lock_path).expect("first lock acquire");

        // Second lock attempt from same/another process should be blocked or fail
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let try_res = f.try_lock();
        assert!(
            try_res.is_err(),
            "second lock acquire must fail when lock is held"
        );

        drop(lock1);
        f.try_lock()
            .expect("lock acquire succeeds after first lock released");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n4_deleted_brief_delivers_full_content_from_spool() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n4-deleted-brief-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(
            &brief_path,
            "## Goal\nRecoverable goal\n## Constraints\n- Context: ctx\n- Decision: argon2\n- Decision: strict\n## Last turn (outgoing harness)\nFinal words\n",
        )
        .unwrap();
        let brief = BriefPair {
            brief_path: brief_path.clone(),
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        // 1. Record while backend is offline -> spooled
        let offline_url = offline_server_url();
        let dest = record_handoff_to(
            &SessionId::new("sess-n4-deleted"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "project-n4",
            &offline_url,
            None,
            &dir,
        )
        .await
        .unwrap();
        assert_eq!(dest, HandoffDestination::Spooled);

        // 2. Delete the brief file completely from disk!
        std::fs::remove_file(&brief_path).unwrap();
        assert!(!brief_path.exists());

        // 3. Start server and drain spool
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-n4"}}"#,
            "application/json",
            2,
        )
        .await;
        let server_url = format!("http://127.0.0.1:{port}");
        let spool_path = dir.join("handoffs.jsonl");

        let drained = drain_spool(&server_url, None, &spool_path, 10)
            .await
            .unwrap();
        assert_eq!(drained, 1);

        // 4. Verify delivered payload carries the full content (goal, decisions, project identity)
        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        let req = &recorded[0];
        assert!(
            req.contains("Recoverable goal"),
            "payload must carry original goal: {req}"
        );
        assert!(
            req.contains("argon2"),
            "payload must carry original decisions: {req}"
        );
        assert!(
            req.contains("project-n4"),
            "payload must carry original project identity: {req}"
        );
        assert!(
            req.contains("Final words"),
            "payload must carry last output: {req}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n4_online_delivery_carries_decisions_and_identity() {
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-n4-online"}}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n4-online-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(
            &brief_path,
            "## Goal\nOnline handoff\n## Constraints\n- Context: ctx\n- Decision: sha256\n## Last turn (outgoing harness)\nDone\n",
        )
        .unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let server_url = format!("http://127.0.0.1:{port}");
        let dest = record_handoff_to(
            &SessionId::new("sess-n4-online"),
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "project-n4-online",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Delivered);

        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        let req = &recorded[0];
        assert!(req.contains("\"project\":\"project-n4-online\""));
        assert!(req.contains("\"next_steps\":[\"Decision: sha256\"]"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n5_append_over_limit_returns_spool_full_error() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n5-limit-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let spool_path = dir.join("handoffs.jsonl");

        // Pre-create spool at the 10 MiB limit
        ensure_spool_dir(&dir).unwrap();
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&spool_path)
                .unwrap();
            let padding = vec![b' '; MAX_SPOOL_BYTES as usize];
            f.write_all(&padding).unwrap();
            f.sync_all().unwrap();
        }

        let rec = SpooledRecord::new(
            "s-over",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g",
            vec![],
            "sum",
        );
        let res = append_spool(&spool_path, &rec);

        assert!(res.is_err(), "append over limit must return error");
        match res.unwrap_err() {
            MemoryError::SpoolFull(msg) => {
                assert!(msg.contains("spool size limit"));
            }
            other => panic!("expected SpoolFull, got: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n5_timed_retry_drain_without_new_handoff() {
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-retry"}}"#,
            "application/json",
            3,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n5-timed-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let spool_path = dir.join("handoffs.jsonl");

        let rec1 = SpooledRecord::new(
            "s1",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g1",
            vec![],
            "sum1",
        );
        let rec2 = SpooledRecord::new(
            "s2",
            "claude-code",
            "codex",
            "p",
            "0Z",
            "g2",
            vec![],
            "sum2",
        );
        append_spool(&spool_path, &rec1).unwrap();
        append_spool(&spool_path, &rec2).unwrap();

        // Run independent public drain with NO new handoff
        let server_url = format!("http://127.0.0.1:{port}");
        let drained = crate::record::drain_spooled_handoffs_to(&server_url, None, &dir, 10)
            .await
            .unwrap();

        assert_eq!(drained, 2);
        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].contains("g1"));
        assert!(recorded[1].contains("g2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn n10_real_session_path_layout_uses_passed_project_identity() {
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"h-n10"}}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-n10-path-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // Real session worktree layout: .../worktrees/<session-id>/.aihub/handoffs/01.md
        let session_id = "sess-uuid-777-abc";
        let worktree = dir.join("worktrees").join(session_id);
        let handoffs_dir = worktree.join(".aihub").join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nReal session path test\n").unwrap();
        let brief = BriefPair {
            brief_path,
            prompt_path: handoffs_dir.join("01.prompt.md"),
        };

        let sid = SessionId::new(session_id);
        let server_url = format!("http://127.0.0.1:{port}");
        let passed_repo_identity = "my-originating-repo";

        let dest = record_handoff_to(
            &sid,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            passed_repo_identity,
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Delivered);

        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        let req = &recorded[0];
        // Must record passed repository identity, NOT the session-id directory name
        assert!(
            req.contains("\"project\":\"my-originating-repo\""),
            "request must carry passed project identity: {req}"
        );
        assert!(
            !req.contains("\"project\":\"sess-uuid-777-abc\""),
            "request must NOT use cwd session basename as project: {req}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_spool_permissions_0700_dir_0600_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-perms-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let spool_path = dir.join("handoffs.jsonl");
        let rec = SpooledRecord::new("s", "claude-code", "codex", "p", "0Z", "g", vec![], "sum");
        append_spool(&spool_path, &rec).unwrap();

        let dir_meta = std::fs::metadata(&dir).unwrap();
        let dir_mode = dir_meta.permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "directory mode must be 0700");

        let spool_meta = std::fs::metadata(&spool_path).unwrap();
        let spool_mode = spool_meta.permissions().mode() & 0o777;
        assert_eq!(spool_mode, 0o600, "spool file mode must be 0600");

        let lock_path = dir.join("handoffs.lock");
        let lock_meta = std::fs::metadata(&lock_path).unwrap();
        let lock_mode = lock_meta.permissions().mode() & 0o777;
        assert_eq!(lock_mode, 0o600, "lock file mode must be 0600");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r4_owned_drain_silent_peer_permits_simultaneous_appends_and_second_drain_with_responsive_timer(
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_url = format!("http://127.0.0.1:{port}");

        let (server_entered_tx, server_entered_rx) = tokio::sync::oneshot::channel();
        let (release_server_tx, release_server_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            // First connection from Drain 1: held until released
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = server_entered_tx.send(());
                let _ = release_server_rx.await;
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 58\r\nConnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"handoff_id\":\"delayed\"}}";
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.flush().await;
            }
            // Second connection from Drain 2: immediate response
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 60\r\nConnection: close\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"handoff_id\":\"immediate\"}}";
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r4-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let spool_path = dir.join("handoffs.jsonl");

        let rec1 = SpooledRecord::new(
            "s1",
            "claude-code",
            "codex",
            "proj",
            "1Z",
            "g1",
            vec![],
            "sum1",
        );
        append_spool(&spool_path, &rec1).unwrap();

        // 1. Launch owned drain against the silent peer in background task
        let sp_clone = spool_path.clone();
        let srv_url = server_url.clone();
        let drain_handle =
            tokio::spawn(async move { drain_spool(&srv_url, None, &sp_clone, 10).await });

        // Wait until server has received connection from drain 1
        server_entered_rx.await.unwrap();

        // 2. While drain 1 is awaiting the slow/silent server, simultaneous appends MUST succeed without blocking!
        let rec2 = SpooledRecord::new(
            "s2",
            "claude-code",
            "codex",
            "proj",
            "2Z",
            "g2",
            vec![],
            "sum2",
        );
        let append_start = Instant::now();
        append_spool_async(&spool_path, &rec2)
            .await
            .expect("simultaneous append must succeed");
        assert!(
            append_start.elapsed() < Duration::from_millis(500),
            "append took {:?}, should not be blocked by network await in drain",
            append_start.elapsed()
        );

        // 3. Second drain serializes asynchronously via DRAIN_ASYNC_MUTEX
        let sp_clone2 = spool_path.clone();
        let srv_url2 = server_url.clone();
        let drain_handle2 =
            tokio::spawn(async move { drain_spool(&srv_url2, None, &sp_clone2, 10).await });

        // 4. Timer responsiveness check: executor threads are not blocked
        let timer_start = Instant::now();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            timer_start.elapsed() < Duration::from_millis(200),
            "timer elapsed {:?}, executor must stay responsive",
            timer_start.elapsed()
        );

        // Release server and await both drain tasks
        let _ = release_server_tx.send(());
        let res1 = drain_handle.await.unwrap().unwrap();
        assert_eq!(res1, 1);
        let res2 = drain_handle2.await.unwrap().unwrap();
        assert_eq!(res2, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r5_null_result_rejected_and_spooled() {
        let (port, _) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r5-null-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest null\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-r5-null");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-r5",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(
            dest,
            HandoffDestination::Spooled,
            "result: null must be rejected and spooled"
        );
        let spool_file = dir.join("handoffs.jsonl");
        assert!(spool_file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r5_scalar_result_rejected_and_spooled() {
        let (port, _) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":"scalar_string"}"#,
            "application/json",
            1,
        )
        .await;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r5-scalar-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest scalar\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-r5-scalar");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-r5",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(
            dest,
            HandoffDestination::Spooled,
            "scalar result must be rejected and spooled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r5_sse_notification_before_result_delivered() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let sse_body = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\r\n\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"handoff_id\":\"h-sse-notif\"}}\r\n\r\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_body.len(),
                    sse_body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r5-notif-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest notif\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-r5-notif");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-r5",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(
            dest,
            HandoffDestination::Delivered,
            "notification before result must deliver successfully"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r5_sse_multiline_and_multiple_events_delivered() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let sse_body = "event: message\r\ndata: {\r\ndata: \"jsonrpc\": \"2.0\",\r\ndata: \"id\": 1,\r\ndata: \"result\": {\"handoff_id\": \"h-multi\"}\r\ndata: }\r\n\r\nevent: ping\r\ndata: {}\r\n\r\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse_body.len(),
                    sse_body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r5-multi-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest multiline\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-r5-multi");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-r5",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(
            dest,
            HandoffDestination::Delivered,
            "multiline SSE data must deliver successfully"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r5_chunked_response_delivered() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let chunk1 = "{\"jsonrpc\":\"2.0\",";
                let chunk2 = "\"id\":1,\"result\":{\"handoff_id\":\"h-chunk\"}}";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                    chunk1.len(), chunk1, chunk2.len(), chunk2
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r5-chunked-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest chunked\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-r5-chunked");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-r5",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(
            dest,
            HandoffDestination::Delivered,
            "chunked HTTP response must decode and deliver"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r5_open_sse_stream_returns_immediately_after_result() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let sse_body = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"handoff_id\":\"h-stream\"}}\r\n\r\n";
                let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
                let _ = socket.write_all(headers.as_bytes()).await;
                let _ = socket.write_all(sse_body.as_bytes()).await;
                let _ = socket.flush().await;
                // Keep socket open until test signals shutdown or 10s
                tokio::select! {
                    _ = &mut shutdown_rx => {},
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {},
                }
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r5-open-sse-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest open stream\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-r5-open-sse");
        let server_url = format!("http://127.0.0.1:{port}");

        let start = Instant::now();
        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-r5",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(dest, HandoffDestination::Delivered);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "open SSE stream must return immediately after complete result, took {:?}",
            start.elapsed()
        );
        let _ = shutdown_tx.send(());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r5_oversized_headers_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                // Send 84 KiB of headers without double CRLF
                let spam_header = "X-Foo: bar\r\n".repeat(7000);
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\n").await;
                let _ = socket.write_all(spam_header.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r5-headers-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let handoffs_dir = dir.join("handoffs");
        std::fs::create_dir_all(&handoffs_dir).unwrap();
        let brief_path = handoffs_dir.join("01.md");
        std::fs::write(&brief_path, "## Goal\nTest oversized headers\n").unwrap();
        let prompt_path = handoffs_dir.join("01.prompt.md");
        std::fs::write(&prompt_path, "prompt").unwrap();

        let brief = BriefPair {
            brief_path,
            prompt_path,
        };
        let session_id = SessionId::new("sess-r5-headers");
        let server_url = format!("http://127.0.0.1:{port}");

        let dest = record_handoff_to(
            &session_id,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            &brief,
            "proj-r5",
            &server_url,
            None,
            &dir,
        )
        .await
        .unwrap();

        assert_eq!(
            dest,
            HandoffDestination::Spooled,
            "oversized headers must be rejected and spooled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r6_partial_write_restart_and_recovery() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r6-partial-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_spool_dir(&dir).unwrap();
        let spool_path = dir.join("handoffs.jsonl");

        let rec1 = SpooledRecord::new(
            "s1",
            "claude-code",
            "codex",
            "proj",
            "1Z",
            "g1",
            vec![],
            "sum1",
        );
        append_spool(&spool_path, &rec1).unwrap();

        // Simulate crash / power loss / full disk during append leaving truncated line without newline
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&spool_path)
                .unwrap();
            write!(f, "{{\"schema_version\":1,\"session_id\":\"incomplete-turn").unwrap();
            f.sync_all().unwrap();
        }

        // Next append must recover from partial tail, quarantine corrupt partial line, and succeed
        let rec2 = SpooledRecord::new(
            "s2",
            "claude-code",
            "codex",
            "proj",
            "2Z",
            "g2",
            vec![],
            "sum2",
        );
        append_spool(&spool_path, &rec2).unwrap();

        // Quarantine file must exist and contain the incomplete line
        let quarantine_path = dir.join("handoffs.quarantine.jsonl");
        assert!(quarantine_path.exists(), "quarantine file must exist");
        let q_content = std::fs::read_to_string(&quarantine_path).unwrap();
        assert!(q_content.contains("incomplete-turn"));

        // Both valid records must now drain successfully to server
        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"ok"}}"#,
            "application/json",
            2,
        )
        .await;
        let server_url = format!("http://127.0.0.1:{port}");

        let drained = drain_spool(&server_url, None, &spool_path, 10)
            .await
            .unwrap();
        assert_eq!(
            drained, 2,
            "both valid records must be delivered after tail recovery"
        );
        assert_eq!(reqs.lock().await.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r6_malformed_head_delivers_later_records_and_quarantines() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r6-head-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_spool_dir(&dir).unwrap();
        let spool_path = dir.join("handoffs.jsonl");

        let rec_valid = SpooledRecord::new(
            "s-valid",
            "claude-code",
            "codex",
            "proj",
            "1Z",
            "g-valid",
            vec![],
            "sum",
        );
        let valid_line = serde_json::to_string(&rec_valid).unwrap();

        // Write malformed line followed by valid record
        let content = format!("{{\"corrupt_json\": true\n{valid_line}\n");
        std::fs::write(&spool_path, content).unwrap();

        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"ok"}}"#,
            "application/json",
            1,
        )
        .await;
        let server_url = format!("http://127.0.0.1:{port}");

        let drained = drain_spool(&server_url, None, &spool_path, 10)
            .await
            .unwrap();
        assert_eq!(
            drained, 1,
            "valid record after malformed head must be delivered"
        );

        let quarantine_path = dir.join("handoffs.quarantine.jsonl");
        assert!(quarantine_path.exists());
        let q_content = std::fs::read_to_string(&quarantine_path).unwrap();
        assert!(q_content.contains("corrupt_json"));

        assert_eq!(reqs.lock().await.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn r6_old_schema_record_migrates_and_delivers() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r6-old-schema-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        ensure_spool_dir(&dir).unwrap();
        let spool_path = dir.join("handoffs.jsonl");

        // Old legacy schema without schema_version and with brief_path
        let legacy_json = r#"{"session_id":"s-legacy-99","from_harness":"claude-code","to_harness":"codex","project":"proj-old","recorded_at":"9999Z","brief_path":null}"#;
        std::fs::write(&spool_path, format!("{legacy_json}\n")).unwrap();

        let (port, reqs) = start_fake_server(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"handoff_id":"ok-migrated"}}"#,
            "application/json",
            1,
        )
        .await;
        let server_url = format!("http://127.0.0.1:{port}");

        let drained = drain_spool(&server_url, None, &spool_path, 10)
            .await
            .unwrap();
        assert_eq!(drained, 1, "legacy schema record must migrate and deliver");

        let recorded = reqs.lock().await;
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].contains("\"project\":\"proj-old\""));
        assert!(recorded[0].contains("s-legacy-99"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn r7_first_oversized_record_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r7-oversized-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let spool_path = dir.join("handoffs.jsonl");
        assert!(!spool_path.exists(), "spool file must not exist yet");

        let huge_goal = "x".repeat(11 * 1024 * 1024);
        let rec = SpooledRecord::new(
            "s-huge",
            "claude-code",
            "codex",
            "proj",
            "1Z",
            huge_goal,
            vec![],
            "sum",
        );

        let res = append_spool(&spool_path, &rec);
        match res {
            Err(MemoryError::SpoolFull(msg)) => {
                assert!(msg.contains("exceeds maximum spool capacity"));
            }
            other => panic!("expected SpoolFull, got: {other:?}"),
        }
        assert!(
            !spool_path.exists(),
            "oversized record must not create spool file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn r7_existing_permissive_file_corrected_or_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r7-perms-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let spool_path = dir.join("handoffs.jsonl");
        std::fs::write(&spool_path, "").unwrap();
        std::fs::set_permissions(&spool_path, std::fs::Permissions::from_mode(0o666)).unwrap();

        // open_secure_file must correct 0o666 to 0o600
        let file = open_secure_file(&spool_path, true, true, false, false).unwrap();
        let meta = file.metadata().unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "permissive file must be corrected to 0600");

        // Refuse symlinks
        let symlink_path = dir.join("handoffs.symlink");
        std::os::unix::fs::symlink(&spool_path, &symlink_path).unwrap();
        let sym_res = open_secure_file(&symlink_path, true, true, false, false);
        match sym_res {
            Err(MemoryError::UnsafePermissions(msg)) => {
                assert!(msg.contains("refusing symlinked target"));
            }
            other => panic!("expected UnsafePermissions for symlink, got: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn r7_denied_permission_propagates_error() {
        let dir = std::env::temp_dir().join(format!(
            "aihub-test-r7-denied-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Chmod seam simulates permission denied (EPERM)
        let chmod_fail = |_p: &Path| -> std::io::Result<()> {
            Err(std::io::Error::from_raw_os_error(libc::EPERM))
        };

        let res = ensure_spool_dir_with_seam(&dir, Some(&chmod_fail));
        match res {
            Err(MemoryError::Io(e)) => {
                assert_eq!(e.raw_os_error(), Some(libc::EPERM));
            }
            other => panic!("expected MemoryError::Io with EPERM, got: {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
