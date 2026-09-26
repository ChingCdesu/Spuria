//! Short-lived relay admission tickets authenticated with HMAC-SHA256.
//!
//! Only signaling and relay servers hold this dedicated key. Clients treat the
//! ticket as opaque. The relay must additionally consume each ticket once;
//! signature verification alone does not prevent replay.

use crate::{Error, Result};
use rand::RngCore;
use ring::hmac;
use std::time::{SystemTime, UNIX_EPOCH};

pub const TICKET_LIFETIME_SECS: u64 = 120;
/// Allow small signaling/relay clock differences without extending expiry.
pub const CLOCK_SKEW_SECS: u64 = 30;
const DOMAIN: &[u8] = b"spuria-relay-ticket-v1:";
const PAYLOAD_LEN: usize = 1 + 8 + 16;
pub const MAX_TICKET_LEN: usize = (PAYLOAD_LEN + 32) * 2;

/// A validated ticket's expiry. The complete ticket uniquely identifies its
/// single rendezvous; a random 128-bit nonce prevents collisions.
pub struct TicketClaims {
    pub expires_at: u64,
}

pub struct RelayTicketKey(hmac::Key);

impl RelayTicketKey {
    pub fn new(secret: &str) -> Result<Self> {
        if secret.trim().len() < 32 {
            return Err(Error::Key(
                "relay secret must contain at least 32 bytes".into(),
            ));
        }
        Ok(Self(hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes())))
    }

    pub fn issue(&self) -> Result<String> {
        self.issue_at(unix_now()?)
    }

    fn issue_at(&self, now: u64) -> Result<String> {
        let expiry = now
            .checked_add(TICKET_LIFETIME_SECS)
            .ok_or_else(|| Error::Protocol("relay ticket expiry overflow".into()))?;
        let mut payload = [0u8; PAYLOAD_LEN];
        payload[0] = 1;
        payload[1..9].copy_from_slice(&expiry.to_be_bytes());
        rand::thread_rng().fill_bytes(&mut payload[9..]);
        let mut signed = Vec::from(DOMAIN);
        signed.extend_from_slice(&payload);
        let tag = hmac::sign(&self.0, &signed);
        let mut ticket = payload.to_vec();
        ticket.extend_from_slice(tag.as_ref());
        Ok(hex::encode(ticket))
    }

    pub fn verify(&self, ticket: &str) -> Result<TicketClaims> {
        self.verify_at(ticket, unix_now()?)
    }

    /// Verify against the server's admission clock. A relay can maintain a
    /// nondecreasing wall-clock watermark so expired tickets never revive
    /// after a backward clock adjustment.
    pub fn verify_at(&self, ticket: &str, now: u64) -> Result<TicketClaims> {
        // Canonical lowercase encoding also prevents aliases bypassing a
        // replay ledger keyed by the opaque ticket string.
        if ticket.len() != MAX_TICKET_LEN
            || !ticket
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(Error::Auth("invalid relay ticket encoding".into()));
        }
        let bytes = hex::decode(ticket).map_err(|_| Error::Auth("invalid relay ticket".into()))?;
        let (payload, tag) = bytes.split_at(PAYLOAD_LEN);
        let mut signed = Vec::from(DOMAIN);
        signed.extend_from_slice(payload);
        hmac::verify(&self.0, &signed, tag)
            .map_err(|_| Error::Auth("invalid relay ticket signature".into()))?;
        let expires_at = u64::from_be_bytes(payload[1..9].try_into().unwrap());
        if payload[0] != 1
            || expires_at <= now
            || expires_at.saturating_sub(now) > TICKET_LIFETIME_SECS + CLOCK_SKEW_SECS
        {
            return Err(Error::Auth(
                "relay ticket expired or invalid validity window".into(),
            ));
        }
        Ok(TicketClaims { expires_at })
    }
}

pub fn unix_now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| Error::Protocol("system clock precedes Unix epoch".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    const SECRET: &str = "unit-test-relay-secret-32-bytes-long";

    #[test]
    fn tickets_require_authentic_signature_and_valid_window() {
        let key = RelayTicketKey::new(SECRET).unwrap();
        let ticket = key.issue_at(1000).unwrap();
        assert_eq!(key.verify_at(&ticket, 1000).unwrap().expires_at, 1120);
        assert!(key.verify_at(&ticket, 1120).is_err());
        assert!(key.verify_at(&ticket, 999).is_ok());
        assert!(key.verify_at(&ticket, 970).is_ok());
        assert!(key.verify_at(&ticket, 969).is_err());
        assert!(RelayTicketKey::new("different-relay-secret-32-bytes-long")
            .unwrap()
            .verify_at(&ticket, 1000)
            .is_err());
        let mut changed = ticket.clone().into_bytes();
        changed[20] = if changed[20] == b'0' { b'1' } else { b'0' };
        assert!(key
            .verify_at(std::str::from_utf8(&changed).unwrap(), 1000)
            .is_err());
        assert!(key.verify_at(&ticket.to_uppercase(), 1000).is_err());
        assert!(key.verify_at("invented-ticket", 1000).is_err());
    }

    #[test]
    fn tickets_are_unique_and_weak_keys_are_rejected() {
        let key = RelayTicketKey::new(SECRET).unwrap();
        assert_ne!(key.issue_at(1000).unwrap(), key.issue_at(1000).unwrap());
        assert!(RelayTicketKey::new("").is_err());
        assert!(RelayTicketKey::new(&" ".repeat(32)).is_err());
    }
}
