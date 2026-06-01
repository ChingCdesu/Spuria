//! Self-signed QUIC certificate + fingerprint pinning (plan §2: "证书钉对端
//! 公钥", §6 "中间人攻击").
//!
//! Each client generates an ephemeral self-signed cert at startup and ships its
//! SHA-256 fingerprint in the [`SessionOffer`]. The connecting side pins that
//! fingerprint via a custom rustls verifier, so a relay or on-path attacker
//! cannot impersonate the peer even though the cert is otherwise untrusted.
//!
//! [`SessionOffer`]: spuria_common::candidate::SessionOffer

use anyhow::Result;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use spuria_common::crypto::cert_fingerprint;

/// A generated certificate kept as DER bytes so we can hand fresh owned copies
/// to rustls per connection (its key types are not cheaply cloneable).
pub struct SelfSignedCert {
    cert_der: Vec<u8>,
    key_pkcs8: Vec<u8>,
    pub fingerprint: String,
}

impl SelfSignedCert {
    pub fn rustls_cert(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.cert_der.clone())
    }

    pub fn rustls_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.clone()))
    }
}

/// Generate a fresh self-signed certificate for the "spuria" SAN.
pub fn generate() -> Result<SelfSignedCert> {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["spuria".to_string()])?;
    let cert_der = cert.der().to_vec();
    let fingerprint = cert_fingerprint(&cert_der);
    Ok(SelfSignedCert {
        cert_der,
        key_pkcs8: key_pair.serialize_der(),
        fingerprint,
    })
}
