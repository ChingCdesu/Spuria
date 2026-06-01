//! ICE-style connectivity candidates (plan §3.3 "候选收集").

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// How a candidate address was learned. Priority ordering mirrors ICE:
/// host > server-reflexive > relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CandidateKind {
    /// Address of a local network interface.
    Host,
    /// Public mapping observed by the reflection ("srflx") service.
    Srflx,
    /// Relay-assigned address (last resort).
    Relay,
}

impl CandidateKind {
    /// Base priority used to order punch/probe attempts (higher = try first).
    pub fn base_priority(self) -> u32 {
        match self {
            CandidateKind::Host => 126,
            CandidateKind::Srflx => 100,
            CandidateKind::Relay => 0,
        }
    }
}

/// A single connectivity candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub addr: SocketAddr,
}

impl Candidate {
    pub fn new(kind: CandidateKind, addr: SocketAddr) -> Self {
        Self { kind, addr }
    }

    pub fn priority(&self) -> u32 {
        self.kind.base_priority()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_orders_host_srflx_relay() {
        assert!(CandidateKind::Host.base_priority() > CandidateKind::Srflx.base_priority());
        assert!(CandidateKind::Srflx.base_priority() > CandidateKind::Relay.base_priority());
    }

    #[test]
    fn candidate_serde_roundtrip() {
        let c = Candidate::new(CandidateKind::Srflx, "1.2.3.4:5".parse().unwrap());
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Candidate>(&json).unwrap(), c);
    }
}

/// Everything one peer needs to attempt a direct connection to the other:
/// its candidate addresses plus the fingerprint pinning its QUIC certificate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionOffer {
    pub candidates: Vec<Candidate>,
    /// SHA-256 (hex) of the peer's self-signed QUIC certificate DER.
    pub quic_cert_fp: String,
}
