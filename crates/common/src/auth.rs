//! Pluggable authentication (plan §2 row "鉴权", decision #3).
//!
//! v1 ships a shared-secret authenticator (device id + team password). The
//! [`Authenticator`] trait is the seam an SSO adapter plugs into later.

use crate::{ids::DeviceId, Error, Result};

/// Credentials a client presents at registration / connection time.
#[derive(Clone, Debug)]
pub struct Credentials {
    pub device_id: DeviceId,
    /// Shared secret / password / bearer token, depending on the backend.
    pub secret: String,
}

/// The authentication seam. Implementations must be cheap and thread-safe so
/// the signaling server can call them on every control-plane request.
pub trait Authenticator: Send + Sync + 'static {
    /// Returns `Ok(())` when the credentials are accepted.
    fn authenticate(&self, creds: &Credentials) -> Result<()>;
}

/// One password for the whole team — the pragmatic v1 model for ~15 people.
pub struct SharedSecretAuth {
    secret: String,
}

impl SharedSecretAuth {
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
        }
    }
}

impl Authenticator for SharedSecretAuth {
    fn authenticate(&self, creds: &Credentials) -> Result<()> {
        // Constant-time-ish compare; secrets are short and low-frequency so a
        // simple length+content check is acceptable for v1.
        if creds.secret.as_bytes().ct_eq(self.secret.as_bytes()) {
            Ok(())
        } else {
            Err(Error::Auth("invalid device secret".into()))
        }
    }
}

/// Development-only: accepts everyone. Never enable in production.
pub struct AllowAllAuth;

impl Authenticator for AllowAllAuth {
    fn authenticate(&self, _creds: &Credentials) -> Result<()> {
        Ok(())
    }
}

/// Per-device credentials loaded from a file — a concrete example of the
/// pluggable seam an SSO adapter would replace. File format: one
/// `device_id:token` pair per line; blank lines and `#` comments are ignored.
pub struct TokenFileAuth {
    creds: std::collections::HashMap<DeviceId, String>,
}

impl TokenFileAuth {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut creds = std::collections::HashMap::new();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (id, token) = line.split_once(':').ok_or_else(|| {
                Error::Auth(format!(
                    "auth file line {}: expected device_id:token",
                    lineno + 1
                ))
            })?;
            creds.insert(DeviceId::new(id.trim()), token.trim().to_string());
        }
        Ok(Self { creds })
    }

    pub fn len(&self) -> usize {
        self.creds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.creds.is_empty()
    }
}

impl Authenticator for TokenFileAuth {
    fn authenticate(&self, creds: &Credentials) -> Result<()> {
        match self.creds.get(&creds.device_id) {
            Some(token) if token.as_bytes().ct_eq(creds.secret.as_bytes()) => Ok(()),
            _ => Err(Error::Auth("unknown device or invalid token".into())),
        }
    }
}

/// Minimal constant-time byte comparison to avoid trivial timing leaks on the
/// shared secret.
trait CtEq {
    fn ct_eq(&self, other: &Self) -> bool;
}

impl CtEq for [u8] {
    fn ct_eq(&self, other: &[u8]) -> bool {
        if self.len() != other.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in self.iter().zip(other.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(secret: &str) -> Credentials {
        Credentials {
            device_id: "dev".into(),
            secret: secret.into(),
        }
    }

    #[test]
    fn shared_secret_accepts_and_rejects() {
        let auth = SharedSecretAuth::new("hunter2");
        assert!(auth.authenticate(&creds("hunter2")).is_ok());
        assert!(auth.authenticate(&creds("wrong")).is_err());
        assert!(auth.authenticate(&creds("hunter")).is_err()); // length mismatch
    }

    #[test]
    fn allow_all_accepts_everything() {
        assert!(AllowAllAuth.authenticate(&creds("")).is_ok());
    }

    #[test]
    fn ct_eq_matches_semantics() {
        assert!(b"abc".ct_eq(b"abc"));
        assert!(!b"abc".ct_eq(b"abd"));
        assert!(!b"abc".ct_eq(b"ab"));
    }
}
