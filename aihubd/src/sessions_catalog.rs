//! Catálogo mínimo de sessões em disco (ADR contradição 3, Opção B; §4.3 lacuna).
//!
//! `sessions.json` grava, por sessão ativa, o suficiente para auditar/limpar PIDs órfãos
//! num próximo arranque: o daemon atual não implementa handover de file descriptor (§4.3
//! é lacuna aberta), então um restart é destrutivo para sessões em andamento. O que este
//! catálogo evita é órfão *silencioso*: um processo que sobrevive sob `setsid` ao daemon
//! cair, mas que nenhum `Session` em memória referencia mais depois do restart, ele é
//! terminado de forma graciosa (SIGTERM, depois SIGKILL) e removido — nunca fica vagando.
use aihub_core::{HarnessId, SessionId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: SessionId,
    /// PID do líder do grupo de processos. `None` quando o `Spawner` não expõe um PID real
    /// (ver `remaining` no result.md desta sessão — `aihub-pty` não tem getter público).
    pub pid: Option<i32>,
    pub owner: String,
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub harness: HarnessId,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SessionsCatalog(pub Vec<SessionRecord>);

impl SessionsCatalog {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "[]".to_string());
        std::fs::write(path, json)
    }

    pub fn upsert(&mut self, record: SessionRecord) {
        if let Some(existing) = self
            .0
            .iter_mut()
            .find(|r| r.session_id == record.session_id)
        {
            *existing = record;
        } else {
            self.0.push(record);
        }
    }
}

/// `kill -0 <pid>`: verdadeiro se o processo (ou grupo) ainda responde a sinal, sem
/// depender do crate `libc` (não declarado nas dependências desta fatia — ver result.md).
async fn pid_is_alive(pid: i32) -> bool {
    tokio::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Encerra graciosamente um grupo de processos órfão: SIGTERM, aguarda uma folga curta,
/// escala para SIGKILL se ainda vivo. Nunca é chamado para uma sessão com `Session` em
/// memória — só para PIDs que o catálogo em disco lista e que o arranque atual não
/// reconhece mais (Opção B: sem handover de fd, a reconciliação é encerramento, não
/// reatrelamento).
async fn terminate_orphan_group(pid: i32) {
    let _ = tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(format!("-{pid}"))
        .output()
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    if pid_is_alive(pid).await {
        let _ = tokio::process::Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .output()
            .await;
    }
}

/// Roda uma vez no arranque, antes de aceitar conexões: lê o catálogo persistido, termina
/// graciosamente todo PID ainda vivo (órfão, já que o processo atual não tem `Session` para
/// ele) e grava um catálogo vazio de volta. Retorna os `SessionId` reconciliados, para log.
pub async fn reconcile_startup_catalog(path: &Path) -> Vec<SessionId> {
    let catalog = SessionsCatalog::load(path);
    let mut reconciled = Vec::new();
    for record in &catalog.0 {
        if let Some(pid) = record.pid {
            if pid_is_alive(pid).await {
                terminate_orphan_group(pid).await;
                reconciled.push(record.session_id.clone());
            }
        }
    }
    let _ = SessionsCatalog::default().save(path);
    reconciled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn record(id: &str, pid: Option<i32>) -> SessionRecord {
        SessionRecord {
            session_id: SessionId::new(id),
            pid,
            owner: "local".into(),
            repo_path: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/wt"),
            branch: "session/x".into(),
            harness: HarnessId::ClaudeCode,
        }
    }

    fn test_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "ah04-catalog-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn round_trips_through_disk() {
        let path = test_path();
        let mut catalog = SessionsCatalog::default();
        catalog.upsert(record("a", Some(123)));
        catalog.save(&path).unwrap();
        let loaded = SessionsCatalog::load(&path);
        assert_eq!(loaded.0.len(), 1);
        assert_eq!(loaded.0[0].session_id, SessionId::new("a"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn upsert_replaces_existing_record() {
        let mut catalog = SessionsCatalog::default();
        catalog.upsert(record("a", Some(1)));
        catalog.upsert(record("a", Some(2)));
        assert_eq!(catalog.0.len(), 1);
        assert_eq!(catalog.0[0].pid, Some(2));
    }

    #[tokio::test]
    async fn reconciles_and_terminates_a_live_orphan_process_group() {
        let path = test_path();
        // Spawn a real child in its own process group (`process_group(0)`, stable std, no
        // `libc` dependency needed), so `kill(-pgid)` reaches only it — mirroring the setsid
        // leader `aihub-pty/src/spawn.rs` launches, without inheriting this test's own group.
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("exec sleep 60")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        // `kill -0` still reports a signalled-but-unreaped zombie as "alive" (the PID slot is
        // held until something calls `wait`). A real orphan eventually gets reaped by launchd
        // once its original parent is gone; here that's this test's own job, done on a
        // blocking thread so it doesn't stall the async reconciliation loop below.
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });
        let mut catalog = SessionsCatalog::default();
        catalog.upsert(record("orphan", Some(pid)));
        catalog.save(&path).unwrap();

        assert!(
            pid_is_alive(pid).await,
            "child must be alive before reconciliation"
        );
        let reconciled = reconcile_startup_catalog(&path).await;
        assert_eq!(reconciled, vec![SessionId::new("orphan")]);

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if !pid_is_alive(pid).await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("orphan child must be terminated by reconciliation");

        reaper.join().unwrap();
        let after = SessionsCatalog::load(&path);
        assert!(
            after.0.is_empty(),
            "catalog must be cleared after reconciliation"
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn reconciliation_ignores_dead_pids_without_error() {
        let path = test_path();
        let mut catalog = SessionsCatalog::default();
        // A pid that is almost certainly not alive: reuse this test process's own pid space
        // by picking a very high, implausible value.
        catalog.upsert(record("stale", Some(999_999)));
        catalog.save(&path).unwrap();
        let reconciled = reconcile_startup_catalog(&path).await;
        assert!(reconciled.is_empty());
        std::fs::remove_file(&path).ok();
    }
}
