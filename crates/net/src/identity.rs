//! Device identity: the self-signed certificate and its fingerprint
//! (`app_info.md` §9.1).
//!
//! The fingerprint is the **only** identity in the system. Nothing trusts a
//! self-reported id field, and §9.5 makes the pinned fingerprint pair the
//! credential once pairing has happened.

use photosync_core::model::{Hash32, HASH_LEN};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

/// A generated keypair and certificate.
///
/// Persisting this is the host's job, and it must go into the OS keystore —
/// iOS Keychain with `kSecAttrAccessibleAfterFirstUnlock`, Android Keystore.
/// **Not** shared preferences: LocalSend stores its private key as plaintext
/// JSON in prefs, which means a device backup hands over the identity (§9.1).
#[derive(Clone)]
pub struct Identity {
    cert_der: Vec<u8>,
    key_pkcs8_der: Vec<u8>,
    fingerprint: Hash32,
}

impl Identity {
    /// Generates a fresh identity.
    ///
    /// ECDSA P-256, not RSA-2048. LocalSend uses RSA purely for byte
    /// compatibility with its own older Dart implementation; there is no such
    /// legacy here, and on a phone the difference in handshake cost and key size
    /// is free to take (§9.1).
    ///
    /// No SAN and no expiry management, deliberately: peers are addressed by IP,
    /// so hostname verification is meaningless and is switched off on the client
    /// side (§9.2).
    pub fn generate() -> Result<Self, Error> {
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .map_err(|e| Error::Generate(e.to_string()))?;

        let mut params =
            rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| Error::Generate(e.to_string()))?;
        let mut dn = rcgen::DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, "PhotoSync Device");
        params.distinguished_name = dn;

        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| Error::Generate(e.to_string()))?;

        let cert_der = cert.der().to_vec();
        let fingerprint = fingerprint_of(&cert_der);

        Ok(Self {
            cert_der,
            key_pkcs8_der: key_pair.serialize_der(),
            fingerprint,
        })
    }

    /// Restores an identity persisted by the host.
    pub fn from_der(cert_der: Vec<u8>, key_pkcs8_der: Vec<u8>) -> Self {
        let fingerprint = fingerprint_of(&cert_der);
        Self {
            cert_der,
            key_pkcs8_der,
            fingerprint,
        }
    }

    /// SHA-256 over the certificate DER (§9.1).
    pub const fn fingerprint(&self) -> Hash32 {
        self.fingerprint
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// The private key. Handle as a secret: never log it, never copy it into a
    /// value that might be logged.
    pub fn key_pkcs8_der(&self) -> &[u8] {
        &self.key_pkcs8_der
    }

    pub(crate) fn rustls_cert(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.cert_der.clone())
    }

    pub(crate) fn rustls_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8_der.clone()))
    }
}

// Keep the private key out of any accidental debug output.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// SHA-256 of a certificate's DER encoding.
pub fn fingerprint_of(cert_der: &[u8]) -> Hash32 {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    let out = hasher.finalize();
    let mut bytes = [0u8; HASH_LEN];
    bytes.copy_from_slice(&out);
    Hash32::from_bytes(bytes)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot generate device identity: {0}")]
    Generate(String),
}
