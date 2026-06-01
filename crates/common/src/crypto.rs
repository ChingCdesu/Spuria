//! End-to-end crypto for the relay path (plan §2: `snow` Noise, public keys
//! pinned via signaling) plus helpers for QUIC certificate pinning.
//!
//! The relay only ever sees Noise ciphertext. We use the `KK` handshake
//! pattern: both peers already know each other's static public key (exchanged
//! through the signaling server), which gives us mutual authentication and
//! forward secrecy in two messages.

use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Noise suite. `KK` = both static keys known ahead of time.
pub const NOISE_PARAMS: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";

/// Max plaintext per Noise message (65535 frame − 16 byte AEAD tag).
pub const MAX_PLAINTEXT_CHUNK: usize = 65519;

/// A persistent static keypair identifying a device on the Noise layer.
#[derive(Clone)]
pub struct DeviceKey {
    private: Vec<u8>,
    public: Vec<u8>,
}

impl DeviceKey {
    /// Generate a fresh X25519 static keypair.
    pub fn generate() -> Result<Self> {
        let params = NOISE_PARAMS
            .parse()
            .map_err(|_| Error::Noise("invalid noise params".into()))?;
        let kp = snow::Builder::new(params).generate_keypair()?;
        Ok(Self {
            private: kp.private,
            public: kp.public,
        })
    }

    pub fn public(&self) -> &[u8] {
        &self.public
    }

    pub fn public_hex(&self) -> String {
        hex::encode(&self.public)
    }

    /// Load a key from disk, or generate-and-persist one if absent.
    pub fn load_or_generate(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            let key = Self::generate()?;
            key.save(path)?;
            Ok(key)
        }
    }

    /// On-disk format: raw `private(32) || public(32)`.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut bytes = Vec::with_capacity(self.private.len() + self.public.len());
        bytes.extend_from_slice(&self.private);
        bytes.extend_from_slice(&self.public);
        std::fs::write(path, bytes)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        if bytes.len() != 64 {
            return Err(Error::Key(format!(
                "device key file must be 64 bytes, got {}",
                bytes.len()
            )));
        }
        Ok(Self {
            private: bytes[..32].to_vec(),
            public: bytes[32..].to_vec(),
        })
    }
}

/// Drives the Noise handshake. The caller is responsible for shuttling the
/// returned/consumed byte messages over whatever transport it has.
pub struct Handshake {
    state: snow::HandshakeState,
}

impl Handshake {
    pub fn initiator(local: &DeviceKey, remote_public: &[u8]) -> Result<Self> {
        Ok(Self {
            state: Self::builder(local, remote_public)?.build_initiator()?,
        })
    }

    pub fn responder(local: &DeviceKey, remote_public: &[u8]) -> Result<Self> {
        Ok(Self {
            state: Self::builder(local, remote_public)?.build_responder()?,
        })
    }

    fn builder<'a>(local: &'a DeviceKey, remote_public: &'a [u8]) -> Result<snow::Builder<'a>> {
        let params = NOISE_PARAMS
            .parse()
            .map_err(|_| Error::Noise("invalid noise params".into()))?;
        Ok(snow::Builder::new(params)
            .local_private_key(&local.private)
            .remote_public_key(remote_public))
    }

    /// Produce the next handshake message (no application payload).
    pub fn write(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 65535];
        let n = self.state.write_message(&[], &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume an incoming handshake message.
    pub fn read(&mut self, msg: &[u8]) -> Result<()> {
        let mut buf = vec![0u8; 65535];
        self.state.read_message(msg, &mut buf)?;
        Ok(())
    }

    pub fn is_finished(&self) -> bool {
        self.state.is_handshake_finished()
    }

    pub fn into_session(self) -> Result<NoiseSession> {
        Ok(NoiseSession {
            state: self.state.into_transport_mode()?,
        })
    }
}

/// Established transport-mode Noise session: frames plaintext into
/// length-prefixed ciphertext and back.
pub struct NoiseSession {
    state: snow::TransportState,
}

impl NoiseSession {
    /// Encrypt one chunk into a wire frame: `u16 BE length || ciphertext`.
    pub fn encrypt_frame(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        if plaintext.len() > MAX_PLAINTEXT_CHUNK {
            return Err(Error::Protocol("plaintext chunk too large".into()));
        }
        let mut ct = vec![0u8; plaintext.len() + 16];
        let n = self.state.write_message(plaintext, &mut ct)?;
        ct.truncate(n);
        let mut frame = Vec::with_capacity(2 + n);
        frame.extend_from_slice(&(n as u16).to_be_bytes());
        frame.extend_from_slice(&ct);
        Ok(frame)
    }

    /// Decrypt a single ciphertext (without the 2-byte length prefix) into
    /// plaintext.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut pt = vec![0u8; ciphertext.len().max(16)];
        let n = self.state.read_message(ciphertext, &mut pt)?;
        pt.truncate(n);
        Ok(pt)
    }
}

/// SHA-256 (lowercase hex) of a certificate's DER bytes — the value pinned in
/// [`crate::candidate::SessionOffer::quic_cert_fp`].
pub fn cert_fingerprint(der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(der);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kk_roundtrip() {
        let a = DeviceKey::generate().unwrap();
        let b = DeviceKey::generate().unwrap();

        let mut init = Handshake::initiator(&a, b.public()).unwrap();
        let mut resp = Handshake::responder(&b, a.public()).unwrap();

        // -> e, es, ss
        let m1 = init.write().unwrap();
        resp.read(&m1).unwrap();
        // <- e, ee, se
        let m2 = resp.write().unwrap();
        init.read(&m2).unwrap();

        assert!(init.is_finished() && resp.is_finished());
        let mut sa = init.into_session().unwrap();
        let mut sb = resp.into_session().unwrap();

        let frame = sa.encrypt_frame(b"hello rdp").unwrap();
        // strip the 2-byte length prefix before decrypting
        let pt = sb.decrypt(&frame[2..]).unwrap();
        assert_eq!(pt, b"hello rdp");
    }

    fn established() -> (NoiseSession, NoiseSession) {
        let a = DeviceKey::generate().unwrap();
        let b = DeviceKey::generate().unwrap();
        let mut init = Handshake::initiator(&a, b.public()).unwrap();
        let mut resp = Handshake::responder(&b, a.public()).unwrap();
        let m1 = init.write().unwrap();
        resp.read(&m1).unwrap();
        let m2 = resp.write().unwrap();
        init.read(&m2).unwrap();
        (init.into_session().unwrap(), resp.into_session().unwrap())
    }

    #[test]
    fn wrong_peer_key_fails_handshake() {
        let a = DeviceKey::generate().unwrap();
        let b = DeviceKey::generate().unwrap();
        let imposter = DeviceKey::generate().unwrap();
        // Responder expects `a` but initiator authenticates against `imposter`.
        let mut init = Handshake::initiator(&a, b.public()).unwrap();
        let mut resp = Handshake::responder(&b, imposter.public()).unwrap();
        let m1 = init.write().unwrap();
        assert!(
            resp.read(&m1).is_err(),
            "KK must reject an unexpected static key"
        );
    }

    #[test]
    fn max_chunk_roundtrips_oversize_rejected() {
        let (mut a, mut b) = established();
        let payload = vec![7u8; MAX_PLAINTEXT_CHUNK];
        let frame = a.encrypt_frame(&payload).unwrap();
        let len = u16::from_be_bytes([frame[0], frame[1]]) as usize;
        assert_eq!(len, frame.len() - 2);
        assert_eq!(b.decrypt(&frame[2..]).unwrap(), payload);
        assert!(a
            .encrypt_frame(&vec![0u8; MAX_PLAINTEXT_CHUNK + 1])
            .is_err());
    }

    #[test]
    fn device_key_save_load() {
        let path = std::env::temp_dir().join("spuria-crypto-test-key.bin");
        let _ = std::fs::remove_file(&path);
        let k = DeviceKey::generate().unwrap();
        k.save(&path).unwrap();
        assert_eq!(DeviceKey::load(&path).unwrap().public(), k.public());
        // load_or_generate returns the persisted key, not a new one.
        assert_eq!(
            DeviceKey::load_or_generate(&path).unwrap().public(),
            k.public()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fingerprint_is_sha256_hex() {
        let fp = cert_fingerprint(b"der-bytes");
        assert_eq!(fp.len(), 64);
        assert_eq!(fp, cert_fingerprint(b"der-bytes"));
        assert_ne!(fp, cert_fingerprint(b"other-bytes"));
    }
}
