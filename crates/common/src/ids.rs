use serde::{Deserialize, Serialize};
use std::fmt;

/// A stable, human-shareable device identifier (think RustDesk's 9-digit id).
///
/// We keep it as an opaque string so deployments can choose their own scheme
/// (random digits, hostname-derived, etc.) without a protocol change.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(s: impl Into<String>) -> Self {
        DeviceId(s.into())
    }

    /// Generate a fresh random 9-digit id.
    pub fn random() -> Self {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let n: u64 = rng.gen_range(100_000_000..1_000_000_000);
        DeviceId(n.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.0)
    }
}

impl From<&str> for DeviceId {
    fn from(s: &str) -> Self {
        DeviceId(s.to_string())
    }
}

impl From<String> for DeviceId {
    fn from(s: String) -> Self {
        DeviceId(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_is_nine_digits() {
        let id = DeviceId::random();
        assert_eq!(id.as_str().len(), 9);
        assert!(id.as_str().chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn display_and_eq() {
        assert_eq!(DeviceId::new("abc").to_string(), "abc");
        assert_eq!(DeviceId::from("x"), DeviceId::from("x".to_string()));
    }
}
