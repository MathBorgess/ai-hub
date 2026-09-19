//! Local asymmetric identity for the Mac client (ADR §2.3, §4.5): an ed25519
//! keypair generated once and persisted on disk, used to prove possession of
//! the private key over a daemon-issued challenge during the v3 handshake.
//!
//! The private key never leaves this module. There is no `Debug`/`Display`
//! impl on `Identity` and no accessor returns the signing key or its bytes —
//! only a `ClientCredential` (public key + signature) or a `pairing_code()`
//! fingerprint, so it cannot end up in a log line, banner, or `doctor` output
//! by accident.

use aihub_core::{Base64Bytes, ClientCredential, CREDENTIAL_AUDIENCE};
use anyhow::{Context, Result};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub struct Identity {
    signing_key: SigningKey,
}

impl Identity {
    /// Loads the identity keypair from `path`, generating and persisting a new
    /// one on first run. The key file is written with `0600` permissions.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if let Ok(bytes) = std::fs::read(path) {
            let seed: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("identity file at {:?} is corrupt", path))?;
            return Ok(Self {
                signing_key: SigningKey::from_bytes(&seed),
            });
        }

        // `ed25519_dalek::SigningKey::generate` wants a `rand_core` 0.6
        // `CryptoRngCore`, but this workspace's `rand = "0.10"` is built on an
        // incompatible newer `rand_core` and isn't nameable as that trait —
        // and `rand_core` itself isn't a direct dependency of this crate. A
        // raw random 32-byte seed via the already-used `rand::rng()` (see
        // `aihub_core::SessionId::generate`) needs neither.
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        let signing_key = SigningKey::from_bytes(&seed);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating identity directory")?;
        }
        std::fs::write(path, signing_key.to_bytes()).context("writing identity key")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .context("securing identity key permissions")?;
        }
        Ok(Self { signing_key })
    }

    /// True if an identity file already exists at `path` (pairing bootstrap check,
    /// ADR §4.5: a fresh Mac with no identity file has never been paired).
    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Short human-readable pairing code derived from the public key fingerprint.
    /// Displayed to the owner during first-pairing bootstrap; never derived from
    /// or reversible to the private key.
    pub fn pairing_code(&self) -> String {
        let digest = Sha256::digest(self.public_key().to_bytes());
        let hex: String = digest.iter().take(5).map(|b| format!("{b:02X}")).collect();
        format!("{}-{}", &hex[0..5], &hex[5..10])
    }

    /// Builds a `ClientCredential` proving possession of the private key over
    /// `nonce`, the daemon-issued challenge from `DaemonMessage::Challenge`.
    pub fn credential(&self, nonce: &[u8], timestamp: u64) -> ClientCredential {
        let signature = self.signing_key.sign(nonce);
        ClientCredential {
            public_key: Base64Bytes::new(self.public_key().to_bytes().to_vec()),
            signature: Base64Bytes::new(signature.to_bytes().to_vec()),
            audience: CREDENTIAL_AUDIENCE.to_string(),
            timestamp,
        }
    }
}

/// Default on-disk location for the identity keypair.
pub fn default_identity_path() -> PathBuf {
    aihub_core::default_data_dir().join("identity.key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signature;

    fn scratch_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "aihub-identity-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn load_or_create_persists_and_reuses_keypair() {
        let path = scratch_path("persist");
        let id1 = Identity::load_or_create(&path).unwrap();
        let id2 = Identity::load_or_create(&path).unwrap();
        assert_eq!(id1.public_key().to_bytes(), id2.public_key().to_bytes());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn identity_file_is_owner_only_readable() {
        let path = scratch_path("perms");
        let _id = Identity::load_or_create(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn credential_signature_is_verifiable_over_the_nonce() {
        let path = scratch_path("sign");
        let id = Identity::load_or_create(&path).unwrap();
        let nonce = b"daemon-challenge-nonce";
        let cred = id.credential(nonce, 1_700_000_000);

        assert_eq!(cred.audience, CREDENTIAL_AUDIENCE);
        let sig_bytes: [u8; 64] = cred.signature.as_slice().try_into().unwrap();
        let sig = Signature::from_bytes(&sig_bytes);
        let pk_bytes: [u8; 32] = cred.public_key.as_slice().try_into().unwrap();
        let vk = VerifyingKey::from_bytes(&pk_bytes).unwrap();
        assert!(vk.verify_strict(nonce, &sig).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn credential_over_different_nonce_does_not_verify() {
        let path = scratch_path("wrong-nonce");
        let id = Identity::load_or_create(&path).unwrap();
        let cred = id.credential(b"real-nonce", 0);
        let sig_bytes: [u8; 64] = cred.signature.as_slice().try_into().unwrap();
        let sig = Signature::from_bytes(&sig_bytes);
        let pk_bytes: [u8; 32] = cred.public_key.as_slice().try_into().unwrap();
        let vk = VerifyingKey::from_bytes(&pk_bytes).unwrap();
        assert!(vk.verify_strict(b"different-nonce", &sig).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pairing_code_is_short_and_stable_across_loads() {
        let path = scratch_path("pairing-code");
        let id1 = Identity::load_or_create(&path).unwrap();
        let code1 = id1.pairing_code();
        assert_eq!(code1.len(), 11); // "XXXXX-XXXXX"
        let id2 = Identity::load_or_create(&path).unwrap();
        assert_eq!(code1, id2.pairing_code());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exists_reflects_bootstrap_state() {
        let path = scratch_path("exists");
        assert!(!Identity::exists(&path));
        let _id = Identity::load_or_create(&path).unwrap();
        assert!(Identity::exists(&path));
        let _ = std::fs::remove_file(&path);
    }
}
