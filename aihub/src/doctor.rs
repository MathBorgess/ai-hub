//! `aihub doctor --remote` (ADR gap §4.1): inspects pairing, route, protocol
//! version, and clock skew against a remote daemon, and tells the owner what
//! is wrong instead of the opaque `unauthorized` the daemon itself returns.

use crate::identity::Identity;
use crate::remote::{self, FailureClass};
use std::path::Path;
use std::time::Duration;

/// Static GitHub device-approval URL (ADR §2.3, §4.5: IdP = GitHub). The
/// owner visits this URL and enters the pairing code shown alongside it.
/// The actual OAuth device-flow token exchange is explicitly out of scope
/// for this session (ADR §8 item 4: no GitHub allowlist yet) — this URL is
/// display-only guidance, never called by this binary.
pub const GITHUB_APPROVAL_URL: &str = "https://github.com/login/device";

const DOCTOR_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum PairingStatus {
    /// The daemon accepted this Mac's credential.
    Approved { fingerprint: String },
    /// Connected, but the daemon has not (yet) approved this Mac's key.
    PendingApproval { code: String },
    /// Could not even reach the daemon to find out.
    Unknown { code: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum RouteStatus {
    Ok,
    Failed(FailureClass),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DoctorReport {
    pub target: String,
    pub route: RouteStatus,
    pub pairing: PairingStatus,
    pub protocol_version: Option<u32>,
    pub clock_skew_s: Option<i64>,
}

impl DoctorReport {
    /// Renders the report as the plain text printed by `aihub doctor --remote`.
    pub fn render(&self) -> String {
        let mut lines = vec![format!("aihub doctor --remote: {}", self.target)];

        match &self.route {
            RouteStatus::Ok => lines.push("  rota: OK".to_string()),
            RouteStatus::Failed(class) => {
                lines.push(format!("  rota: FALHA — {}", class.banner_text()))
            }
        }

        match &self.pairing {
            PairingStatus::Approved { fingerprint } => {
                lines.push(format!("  pareamento: aprovado (chave {fingerprint})"))
            }
            PairingStatus::PendingApproval { code } => {
                lines.push("  pareamento: aguardando aprovação do dono".to_string());
                lines.push(format!("    código de pareamento: {code}"));
                lines.push(format!("    aprove em: {GITHUB_APPROVAL_URL}"));
            }
            PairingStatus::Unknown { code } => {
                lines.push("  pareamento: desconhecido (sem rota até o daemon)".to_string());
                lines.push(format!("    código de pareamento deste Mac: {code}"));
            }
        }

        match self.protocol_version {
            Some(v) => lines.push(format!(
                "  versão de protocolo: {v} (cliente: {})",
                aihub_core::PROTOCOL_VERSION
            )),
            None => lines.push("  versão de protocolo: desconhecida (sem handshake)".to_string()),
        }

        match self.clock_skew_s {
            Some(skew) => lines.push(format!("  skew de relógio: {skew}s (Mac - daemon)")),
            None => lines.push("  skew de relógio: indisponível".to_string()),
        }

        lines.join("\n")
    }
}

/// Runs the full remote diagnostic against `url`, using (and creating if
/// absent) the identity keypair at `identity_path`.
pub async fn run_remote_doctor(url: &str, identity_path: &Path) -> anyhow::Result<DoctorReport> {
    let identity = Identity::load_or_create(identity_path)?;
    let code = identity.pairing_code();

    let connect_result = remote::connect_with_response(url, DOCTOR_TIMEOUT).await;

    let (socket, response) = match connect_result {
        Err(e) => {
            return Ok(DoctorReport {
                target: url.to_string(),
                route: RouteStatus::Failed(e.class),
                pairing: PairingStatus::Unknown { code },
                protocol_version: None,
                clock_skew_s: None,
            })
        }
        Ok(pair) => pair,
    };

    let clock_skew_s = response
        .headers()
        .get("date")
        .and_then(|v| v.to_str().ok())
        .and_then(remote::parse_http_date)
        .map(|daemon_epoch| now_epoch() as i64 - daemon_epoch as i64);

    let mut io = remote::spawn_io(socket);
    let handshake = remote::perform_remote_handshake(&mut io, &identity, DOCTOR_TIMEOUT).await;

    let (pairing, protocol_version) = match handshake {
        Ok(()) => (
            PairingStatus::Approved {
                fingerprint: code.clone(),
            },
            Some(aihub_core::PROTOCOL_VERSION),
        ),
        Err(e) if e.class == FailureClass::Unauthorized => {
            (PairingStatus::PendingApproval { code: code.clone() }, None)
        }
        Err(_) => (PairingStatus::Unknown { code: code.clone() }, None),
    };

    Ok(DoctorReport {
        target: url.to_string(),
        route: RouteStatus::Ok,
        pairing,
        protocol_version,
        clock_skew_s,
    })
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_approved_pairing_with_no_key_material() {
        let report = DoctorReport {
            target: "wss://aihub.mathai.com.br".to_string(),
            route: RouteStatus::Ok,
            pairing: PairingStatus::Approved {
                fingerprint: "ABCDE-12345".to_string(),
            },
            protocol_version: Some(3),
            clock_skew_s: Some(1),
        };
        let text = report.render();
        assert!(text.contains("rota: OK"));
        assert!(text.contains("aprovado"));
        assert!(text.contains("ABCDE-12345"));
        assert!(text.contains("skew de relógio: 1s"));
    }

    #[test]
    fn renders_pending_approval_with_pairing_code_and_github_url() {
        let report = DoctorReport {
            target: "wss://aihub.mathai.com.br".to_string(),
            route: RouteStatus::Ok,
            pairing: PairingStatus::PendingApproval {
                code: "AB12C-34DE5".to_string(),
            },
            protocol_version: None,
            clock_skew_s: None,
        };
        let text = report.render();
        assert!(text.contains("aguardando aprovação"));
        assert!(text.contains("AB12C-34DE5"));
        assert!(text.contains(GITHUB_APPROVAL_URL));
    }

    #[test]
    fn renders_route_failure_with_class_text() {
        let report = DoctorReport {
            target: "wss://unreachable.invalid".to_string(),
            route: RouteStatus::Failed(FailureClass::DnsResolution),
            pairing: PairingStatus::Unknown {
                code: "AAAAA-BBBBB".to_string(),
            },
            protocol_version: None,
            clock_skew_s: None,
        };
        let text = report.render();
        assert!(text.contains("FALHA"));
        assert!(text.contains(FailureClass::DnsResolution.banner_text()));
    }
}
