//! `channel_ticket` de uso único (ADR contradição 1, Opção B).
//!
//! Fecha a junta 01×02: em vez de repetir a prova de posse criptográfica numa segunda
//! conexão, a conexão de controle já autenticada emite um ticket efêmero e amarrado à
//! sessão e ao principal; a conexão de PTY secundária só é admitida se apresentar esse
//! ticket, exatamente uma vez.
use crate::auth::PrincipalId;
use aihub_core::{Base64Bytes, ChannelTicket, SessionId};
use rand::Rng;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Janela de validade de um ticket não redimido. Generosa o bastante para o cliente abrir a
/// segunda conexão TCP/WS em seguida, curta o bastante para não valer como credencial de longa
/// duração.
pub const TICKET_TTL: Duration = Duration::from_secs(30);

struct PendingTicket {
    session_id: SessionId,
    principal: PrincipalId,
    issued_at: Instant,
}

/// Registro em memória de tickets emitidos e ainda não redimidos. Vive dentro do `State`
/// trancado pelo mesmo mutex das sessões — nunca persiste em disco (é efêmero por design).
#[derive(Default)]
pub struct TicketRegistry(HashMap<[u8; 32], PendingTicket>);

impl TicketRegistry {
    /// Emite um novo ticket para `session_id`/`principal`. Nunca reutiliza bytes: CSPRNG de
    /// 256 bits por emissão.
    pub fn issue(&mut self, session_id: SessionId, principal: PrincipalId) -> ChannelTicket {
        self.purge_expired();
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        self.0.insert(
            bytes,
            PendingTicket {
                session_id,
                principal,
                issued_at: Instant::now(),
            },
        );
        ChannelTicket(Base64Bytes::new(bytes.to_vec()))
    }

    /// Redime `ticket` para `session_id`. Uso único: o ticket é removido do registro no
    /// primeiro lookup, válido ou não — uma segunda apresentação do mesmo ticket nunca
    /// encontra nada, mesmo que a primeira tentativa tenha falhado por outro motivo (sessão
    /// errada, por exemplo). Retorna o principal amarrado ao ticket em caso de sucesso.
    pub fn redeem(
        &mut self,
        ticket: &ChannelTicket,
        session_id: &SessionId,
    ) -> Option<PrincipalId> {
        let key: [u8; 32] = ticket.0.as_slice().try_into().ok()?;
        let pending = self.0.remove(&key)?;
        if pending.issued_at.elapsed() > TICKET_TTL {
            return None;
        }
        if &pending.session_id != session_id {
            return None;
        }
        Some(pending.principal)
    }

    fn purge_expired(&mut self) {
        self.0.retain(|_, p| p.issued_at.elapsed() <= TICKET_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redeems_once_then_refuses_reuse() {
        let mut reg = TicketRegistry::default();
        let sid = SessionId::new("sess-1");
        let principal = PrincipalId::local();
        let ticket = reg.issue(sid.clone(), principal.clone());
        assert_eq!(reg.redeem(&ticket, &sid), Some(principal));
        assert_eq!(reg.redeem(&ticket, &sid), None, "reuse must be refused");
    }

    #[test]
    fn refuses_wrong_session() {
        let mut reg = TicketRegistry::default();
        let sid = SessionId::new("sess-1");
        let other = SessionId::new("sess-2");
        let ticket = reg.issue(sid, PrincipalId::local());
        assert_eq!(reg.redeem(&ticket, &other), None);
    }

    #[test]
    fn refuses_unknown_ticket() {
        let mut reg = TicketRegistry::default();
        let sid = SessionId::new("sess-1");
        let bogus = ChannelTicket(Base64Bytes::new(vec![0u8; 32]));
        assert_eq!(reg.redeem(&bogus, &sid), None);
    }

    #[test]
    fn refuses_expired_ticket() {
        let mut reg = TicketRegistry::default();
        let sid = SessionId::new("sess-1");
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        reg.0.insert(
            bytes,
            PendingTicket {
                session_id: sid.clone(),
                principal: PrincipalId::local(),
                issued_at: Instant::now() - TICKET_TTL - Duration::from_secs(1),
            },
        );
        let ticket = ChannelTicket(Base64Bytes::new(bytes.to_vec()));
        assert_eq!(reg.redeem(&ticket, &sid), None);
    }
}
