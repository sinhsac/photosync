//! TLS 1.3 with fingerprint pinning enforced inside the handshake
//! (`app_info.md` §9.2).
//!
//! The one thing to take from LocalSend verbatim: verification happens **during**
//! the handshake, not after the connection is up. A peer presenting the wrong
//! certificate therefore never receives request metadata, let alone photo bytes.
//!
//! What is deliberately *not* copied: LocalSend's discovery client accepts any
//! valid self-signed certificate, and the fingerprint it later pins is learned
//! from that same unauthenticated exchange. That is trust-on-first-use with no
//! first-use authentication. Here, first contact is authenticated by the pairing
//! code instead (§9.3), and [`Pinning::FirstContact`] exists only for that one
//! moment.

use crate::identity::{fingerprint_of, Identity};
use photosync_core::model::Hash32;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::sync::Arc;

/// Whether we already know who we expect to talk to.
#[derive(Clone, Copy, Debug)]
pub enum Pinning {
    /// The peer's fingerprint is known — from `peer.cert_fingerprint` after a
    /// previous pairing (§9.5). Any other certificate fails the handshake.
    ///
    /// This is the normal case for resume and for repeat syncs, and it is why
    /// those need no code.
    Expect(Hash32),

    /// First contact. The fingerprint is not yet known, so it cannot be pinned.
    ///
    /// **This provides no authentication on its own.** It is only safe because
    /// the very next thing that happens is the code-bound proof, whose transcript
    /// commits to both fingerprints, so a substituted certificate makes the proof
    /// fail (§9.3). Never use this variant for a connection that will not
    /// immediately authenticate.
    FirstContact,
}

/// Records the fingerprint the peer actually presented.
///
/// The handshake writes it here so the application layer can feed the real
/// fingerprint into the auth transcript rather than a value the peer claimed
/// about itself.
#[derive(Clone, Debug, Default)]
pub struct ObservedPeer(Arc<std::sync::Mutex<Option<Hash32>>>);

impl ObservedPeer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> Option<Hash32> {
        *self.0.lock().expect("observed peer mutex poisoned")
    }

    fn set(&self, fp: Hash32) {
        *self.0.lock().expect("observed peer mutex poisoned") = Some(fp);
    }
}

/// Client-side verifier.
#[derive(Debug)]
struct PinnedServerVerifier {
    expected: Option<Hash32>,
    observed: ObservedPeer,
    supported: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = fingerprint_of(end_entity.as_ref());
        self.observed.set(actual);

        // Hostname and chain checks are intentionally skipped: the certificate
        // is self-signed with no SAN, and the peer is an IP address on a LAN.
        // The fingerprint is the whole of the identity check (§9.2).
        if let Some(expected) = self.expected {
            if actual != expected {
                return Err(rustls::Error::General(format!(
                    "peer certificate fingerprint {} does not match the pinned {}",
                    actual.short(),
                    expected.short()
                )));
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // TLS 1.2 is not offered (see `protocol_versions`), so this is
        // unreachable. Refuse rather than assert, so a future config change
        // cannot silently weaken anything.
        Err(rustls::Error::General(
            "TLS 1.2 is not supported by PhotoSync".into(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

/// Server-side verifier.
///
/// Mutual TLS is mandatory, so the client must present a certificate. Its
/// fingerprint is recorded and then bound into the auth transcript; the
/// certificate itself is not checked against an allowlist, because at first
/// contact there is nothing to check it against. Authorisation is the code's job
/// (§9.3), not this verifier's.
#[derive(Debug)]
struct RecordingClientVerifier {
    expected: Option<Hash32>,
    observed: ObservedPeer,
    supported: rustls::crypto::WebPkiSupportedAlgorithms,
    empty_dn: Vec<DistinguishedName>,
}

impl ClientCertVerifier for RecordingClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.empty_dn
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let actual = fingerprint_of(end_entity.as_ref());
        self.observed.set(actual);

        if let Some(expected) = self.expected {
            if actual != expected {
                return Err(rustls::Error::General(format!(
                    "client certificate fingerprint {} does not match the pinned {}",
                    actual.short(),
                    expected.short()
                )));
            }
        }
        Ok(ClientCertVerified::assertion())
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General(
            "TLS 1.2 is not supported by PhotoSync".into(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

/// TLS 1.3 only (§9.1). Offering 1.2 would add attack surface for no benefit:
/// both ends are this same app.
static TLS13_ONLY: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];

fn protocol_versions() -> &'static [&'static rustls::SupportedProtocolVersion] {
    TLS13_ONLY
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Builds the client side of a connection.
pub fn client_config(
    identity: &Identity,
    pinning: Pinning,
) -> Result<(Arc<rustls::ClientConfig>, ObservedPeer), rustls::Error> {
    let provider = provider();
    let observed = ObservedPeer::new();
    let verifier = PinnedServerVerifier {
        expected: match pinning {
            Pinning::Expect(fp) => Some(fp),
            Pinning::FirstContact => None,
        },
        observed: observed.clone(),
        supported: provider.signature_verification_algorithms,
    };

    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(protocol_versions())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(vec![identity.rustls_cert()], identity.rustls_key())?;

    Ok((Arc::new(config), observed))
}

/// Builds the server side of a connection.
pub fn server_config(
    identity: &Identity,
    pinning: Pinning,
) -> Result<(Arc<rustls::ServerConfig>, ObservedPeer), rustls::Error> {
    let provider = provider();
    let observed = ObservedPeer::new();
    let verifier = RecordingClientVerifier {
        expected: match pinning {
            Pinning::Expect(fp) => Some(fp),
            Pinning::FirstContact => None,
        },
        observed: observed.clone(),
        supported: provider.signature_verification_algorithms,
        empty_dn: Vec::new(),
    };

    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(protocol_versions())?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_single_cert(vec![identity.rustls_cert()], identity.rustls_key())?;

    Ok((Arc::new(config), observed))
}
