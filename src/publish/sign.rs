//! Author signing for registry uploads.
//!
//! The upload signature binds the artifact identity, not just its bytes, so a
//! registry can reject a token holder who tries to re-label someone else's
//! archive. The private key never leaves the PC.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{pkcs8::DecodePrivateKey, Signer, SigningKey};
use std::fs;

pub struct AuthorSignature {
    /// `ed25519:<base64 public key>` as sent in `x-yam-author-key`.
    pub public_key: String,
    /// Base64 signature over [`upload_message`].
    pub signature: String,
}

/// Load the PKCS#8 PEM key downloaded when the namespace was claimed.
pub fn load_key(path: &std::path::Path) -> Result<SigningKey> {
    SigningKey::from_pkcs8_pem(
        &fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
    )
    .context("author key must be an Ed25519 PKCS#8 PEM private key")
}

pub fn sign_upload(
    key: &SigningKey,
    name: &str,
    version: &str,
    target: &str,
    sha256: &str,
) -> AuthorSignature {
    AuthorSignature {
        public_key: format!(
            "ed25519:{}",
            STANDARD.encode(key.verifying_key().to_bytes())
        ),
        signature: STANDARD.encode(
            key.sign(upload_message(name, version, target, sha256).as_bytes())
                .to_bytes(),
        ),
    }
}

pub fn upload_message(name: &str, version: &str, target: &str, sha256: &str) -> String {
    format!("yamreg-upload-v1\n{name}\n{version}\n{target}\n{sha256}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_signature_binds_all_artifact_identity_fields() {
        assert_eq!(
            upload_message("author/package", "1.2.3", "yandexmini_2", "abc"),
            "yamreg-upload-v1\nauthor/package\n1.2.3\nyandexmini_2\nabc\n"
        );
    }
}
