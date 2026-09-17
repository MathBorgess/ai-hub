//! Verificação de prova de posse, audiência e allowlist para o handshake v3 (ADR §2.3, §6).
//!
//! Este módulo nunca decide se aceita ou recusa uma conexão sozinho: `lib.rs` decide isso
//! por tipo de listener (UDS permissivo / rede fecha-falha, ADR §6). O que este módulo garante
//! é que, quando a rede exige prova, a verificação é positiva (audiência, timestamp, assinatura,
//! allowlist) e que o motivo real da recusa nunca escapa para o chamador — só para o log de
//! auditoria (`02-fronteira-de-confianca.md` §6).
use aihub_core::{ClientCredential, CREDENTIAL_AUDIENCE};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// Janela de tolerância de timestamp do credential (§2.3 "janela de tolerância").
pub const TIMESTAMP_TOLERANCE_SECS: u64 = 300;

/// Identidade amarrada a uma conexão autenticada. `session.owner` é comparado contra isto
/// (01-transporte-e-sessao.md §4) para vetar attach cruzado entre principals.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrincipalId(pub String);

impl PrincipalId {
    /// Identidade única para toda conexão admitida pelo listener Unix local permissivo
    /// (ADR §6): a fronteira de confiança ali é permissão de arquivo, não uma chave, então
    /// todo cliente local compartilha esta mesma identidade — preserva o comportamento atual
    /// de attach livre entre clientes locais.
    pub fn local() -> Self {
        Self("local".into())
    }

    /// Fingerprint used as the principal identity for a network-authenticated connection
    /// (public so integration tests can predict the owner a given keypair will be assigned).
    pub fn from_public_key(key: &[u8; 32]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(key);
        let digest = hasher.finalize();
        Self(digest.iter().take(16).map(|b| format!("{b:02x}")).collect())
    }
}

impl std::fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Motivo interno de recusa. Só alimenta o log de auditoria — o chamador recebe sempre
/// `DaemonMessage::Unauthorized`, sem distinção (ADR §2.3, §6: falha fechada, sem oráculo).
#[derive(Debug, Clone, Copy)]
pub enum AuthReject {
    UnsupportedVersion,
    MissingCredential,
    MalformedKey,
    MalformedSignature,
    BadSignature,
    WrongAudience,
    StaleTimestamp,
    UnknownPrincipal,
}

impl std::fmt::Display for AuthReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AuthReject::UnsupportedVersion => "unsupported protocol version on network listener",
            AuthReject::MissingCredential => "credential missing after challenge",
            AuthReject::MalformedKey => "malformed public key",
            AuthReject::MalformedSignature => "malformed signature",
            AuthReject::BadSignature => "signature did not verify against challenge",
            AuthReject::WrongAudience => "credential audience did not match aihubd",
            AuthReject::StaleTimestamp => "credential timestamp outside tolerance window",
            AuthReject::UnknownPrincipal => "public key not in principal allowlist",
        };
        write!(f, "{s}")
    }
}

/// Conjunto de chaves públicas aprovadas pelo dono (ADR §2.3, "allowlist do principal
/// configurável"). Default vazio: nenhuma chave é confiável até o dono aprovar uma.
#[derive(Default, Clone)]
pub struct Allowlist(HashSet<[u8; 32]>);

impl Allowlist {
    pub fn empty() -> Self {
        Self(HashSet::new())
    }

    /// Carrega de `AIHUB_PRINCIPAL_ALLOWLIST` (chaves base64 separadas por vírgula) se
    /// presente; senão de `AIHUB_PRINCIPAL_ALLOWLIST_FILE` (uma chave base64 por linha,
    /// comentários com `#`); senão vazio. Nunca faz chamada de rede — o pareamento real via
    /// GitHub Device Flow é decisão do dono ainda pendente (ADR §8, item 4).
    pub fn load() -> Self {
        if let Ok(inline) = std::env::var("AIHUB_PRINCIPAL_ALLOWLIST") {
            return Self::parse_lines(inline.split(','));
        }
        let path = std::env::var("AIHUB_PRINCIPAL_ALLOWLIST_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_allowlist_path());
        match std::fs::read_to_string(path) {
            Ok(contents) => Self::parse_lines(contents.lines()),
            Err(_) => Self::empty(),
        }
    }

    fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Self {
        let mut set = HashSet::new();
        for line in lines {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(key) = decode_base64_key(line) {
                set.insert(key);
            }
        }
        Self(set)
    }

    pub fn contains(&self, key: &[u8; 32]) -> bool {
        self.0.contains(key)
    }

    /// Builds an allowlist directly from already-known keys, bypassing `load()`'s env/file
    /// lookup (used by tests, and available to any caller that already has the key material).
    pub fn from_keys(keys: impl IntoIterator<Item = [u8; 32]>) -> Self {
        Self(keys.into_iter().collect())
    }
}

/// Decodes a standard-base64 public key without a direct `base64` dependency: `aihubd` does
/// not depend on that crate itself (only `aihub-core` does), so this reuses
/// `Base64Bytes`'s existing `Deserialize` impl via one JSON string round-trip instead.
fn decode_base64_key(s: &str) -> Option<[u8; 32]> {
    let decoded: aihub_core::Base64Bytes =
        serde_json::from_value(serde_json::Value::String(s.to_string())).ok()?;
    <[u8; 32]>::try_from(decoded.into_inner().as_slice()).ok()
}

pub fn default_allowlist_path() -> PathBuf {
    aihub_core::default_data_dir().join("principals.allowlist")
}

/// Verifica prova de posse e audiência positiva para `credential` sobre o desafio `challenge`
/// (ADR §2.3, §6). A verificação de assinatura roda sempre antes de qualquer retorno, mesmo
/// quando audiência ou timestamp já falharam, para que uma credencial malformada e uma de
/// audiência errada não sejam trivialmente distinguíveis pelo tempo de resposta.
pub fn verify_credential(
    credential: &ClientCredential,
    challenge: &[u8],
    now: u64,
    allowlist: &Allowlist,
) -> Result<PrincipalId, AuthReject> {
    let audience_ok = credential.audience == CREDENTIAL_AUDIENCE;
    let timestamp_ok = now.abs_diff(credential.timestamp) <= TIMESTAMP_TOLERANCE_SECS;
    let key_bytes = <[u8; 32]>::try_from(credential.public_key.as_slice());
    let sig_bytes = <[u8; 64]>::try_from(credential.signature.as_slice());
    let signature_ok = match (&key_bytes, &sig_bytes) {
        (Ok(key), Ok(sig)) => VerifyingKey::from_bytes(key)
            .map(|vk| vk.verify(challenge, &Signature::from_bytes(sig)).is_ok())
            .unwrap_or(false),
        _ => false,
    };
    let key_bytes = key_bytes.map_err(|_| AuthReject::MalformedKey)?;
    sig_bytes.map_err(|_| AuthReject::MalformedSignature)?;
    if !signature_ok {
        return Err(AuthReject::BadSignature);
    }
    if !audience_ok {
        return Err(AuthReject::WrongAudience);
    }
    if !timestamp_ok {
        return Err(AuthReject::StaleTimestamp);
    }
    if !allowlist.contains(&key_bytes) {
        return Err(AuthReject::UnknownPrincipal);
    }
    Ok(PrincipalId::from_public_key(&key_bytes))
}

pub fn audit_log_path() -> PathBuf {
    aihub_core::default_data_dir().join("log/audit.jsonl")
}

/// Escreve uma linha de auditoria durável (ADR §2.3, §6). Nunca grava a credencial em texto
/// claro: só a impressão digital do principal já resolvida, nunca a chave pública crua nem a
/// assinatura recebida.
pub fn audit_log(
    path: &std::path::Path,
    event: &str,
    principal: Option<&PrincipalId>,
    reason: &str,
) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = json!({
        "ts": ts,
        "event": event,
        "principal": principal.map(|p| p.0.clone()),
        "reason": reason,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aihub_core::Base64Bytes;
    use ed25519_dalek::SigningKey;
    use rand::Rng;

    fn random_signing_key() -> SigningKey {
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        SigningKey::from_bytes(&seed)
    }

    fn sign(
        key: &SigningKey,
        challenge: &[u8],
        audience: &str,
        timestamp: u64,
    ) -> ClientCredential {
        use ed25519_dalek::Signer;
        let signature = key.sign(challenge);
        ClientCredential {
            public_key: Base64Bytes::new(key.verifying_key().to_bytes().to_vec()),
            signature: Base64Bytes::new(signature.to_bytes().to_vec()),
            audience: audience.into(),
            timestamp,
        }
    }

    #[test]
    fn accepts_allowlisted_key_with_valid_signature_audience_and_timestamp() {
        let key = random_signing_key();
        let challenge = b"nonce-1";
        let cred = sign(&key, challenge, CREDENTIAL_AUDIENCE, 1000);
        let allow = Allowlist::from_keys([key.verifying_key().to_bytes()]);
        assert!(verify_credential(&cred, challenge, 1000, &allow).is_ok());
    }

    #[test]
    fn rejects_wrong_audience() {
        let key = random_signing_key();
        let challenge = b"nonce-1";
        let cred = sign(&key, challenge, "some-other-service", 1000);
        let allow = Allowlist::from_keys([key.verifying_key().to_bytes()]);
        assert!(matches!(
            verify_credential(&cred, challenge, 1000, &allow),
            Err(AuthReject::WrongAudience)
        ));
    }

    #[test]
    fn rejects_stale_timestamp() {
        let key = random_signing_key();
        let challenge = b"nonce-1";
        let cred = sign(&key, challenge, CREDENTIAL_AUDIENCE, 1000);
        let allow = Allowlist::from_keys([key.verifying_key().to_bytes()]);
        let far_future = 1000 + TIMESTAMP_TOLERANCE_SECS + 1;
        assert!(matches!(
            verify_credential(&cred, challenge, far_future, &allow),
            Err(AuthReject::StaleTimestamp)
        ));
    }

    #[test]
    fn rejects_signature_over_wrong_challenge() {
        let key = random_signing_key();
        let cred = sign(&key, b"nonce-1", CREDENTIAL_AUDIENCE, 1000);
        let allow = Allowlist::from_keys([key.verifying_key().to_bytes()]);
        assert!(matches!(
            verify_credential(&cred, b"nonce-2", 1000, &allow),
            Err(AuthReject::BadSignature)
        ));
    }

    #[test]
    fn rejects_key_not_in_allowlist() {
        let key = random_signing_key();
        let challenge = b"nonce-1";
        let cred = sign(&key, challenge, CREDENTIAL_AUDIENCE, 1000);
        assert!(matches!(
            verify_credential(&cred, challenge, 1000, &Allowlist::empty()),
            Err(AuthReject::UnknownPrincipal)
        ));
    }

    #[test]
    fn different_keys_yield_different_principal_ids() {
        let a = random_signing_key();
        let b = random_signing_key();
        let pa = PrincipalId::from_public_key(&a.verifying_key().to_bytes());
        let pb = PrincipalId::from_public_key(&b.verifying_key().to_bytes());
        assert_ne!(pa, pb);
    }
}
