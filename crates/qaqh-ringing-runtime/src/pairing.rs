//! Short-lived, one-shot pairing tokens shared by Ringing V2 and V3.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::{Scope, device_registry};

/// Default pairing-token lifetime in milliseconds.
pub const DEFAULT_PAIRING_TOKEN_TTL_MS: u64 = 120_000;

#[derive(Debug, Clone)]
pub struct PairingGrant {
    pub scope: Scope,
    pub device_name: String,
    pub platform: String,
    expires_at: Instant,
    consumed: bool,
}

/// In-memory pairing grants keyed by the SHA-256 digest of the plaintext token.
#[derive(Debug)]
pub struct PairingTable {
    grants: HashMap<String, PairingGrant>,
    ttl_ms: u64,
}

impl Default for PairingTable {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingTable {
    pub fn new() -> Self {
        Self::with_ttl_ms(DEFAULT_PAIRING_TOKEN_TTL_MS)
    }

    pub fn with_ttl_ms(ttl_ms: u64) -> Self {
        Self {
            grants: HashMap::new(),
            ttl_ms,
        }
    }

    pub fn ttl_ms(&self) -> u64 {
        self.ttl_ms
    }

    fn purge(&mut self) {
        let now = Instant::now();
        self.grants
            .retain(|_, grant| !grant.consumed && grant.expires_at > now);
    }

    pub fn issue(&mut self, scope: Scope, device_name: String, platform: String) -> String {
        self.purge();
        let token = format!(
            "{}{}",
            qaqh_ringing_contract::ulid::generate_ulid(),
            qaqh_ringing_contract::ulid::generate_ulid()
        );
        let digest = device_registry::token_digest(&token);
        self.grants.insert(
            digest,
            PairingGrant {
                scope,
                device_name,
                platform,
                expires_at: Instant::now() + Duration::from_millis(self.ttl_ms),
                consumed: false,
            },
        );
        token
    }

    /// Consume a grant once, distinguishing used, expired, and unknown tokens.
    pub fn consume(&mut self, token: &str) -> Result<PairingGrant, &'static str> {
        let digest = device_registry::token_digest(token);
        let Some(grant) = self.grants.get(&digest).cloned() else {
            self.purge();
            return Err("pairing_invalid");
        };
        if grant.consumed {
            return Err("pairing_used");
        }
        if grant.expires_at <= Instant::now() {
            self.grants.remove(&digest);
            return Err("pairing_expired");
        }
        if let Some(stored) = self.grants.get_mut(&digest) {
            stored.consumed = true;
        }
        Ok(grant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_ttl_is_reported_and_enforced() {
        let mut table = PairingTable::with_ttl_ms(5);
        assert_eq!(table.ttl_ms(), 5);
        let token = table.issue(Scope::View, "device".into(), "android".into());
        std::thread::sleep(Duration::from_millis(10));

        assert_eq!(table.consume(&token).unwrap_err(), "pairing_expired");
        assert_eq!(table.consume(&token).unwrap_err(), "pairing_invalid");
    }

    #[test]
    fn grants_are_one_shot_and_preserve_requested_identity() {
        let mut table = PairingTable::new();
        assert_eq!(table.ttl_ms(), DEFAULT_PAIRING_TOKEN_TTL_MS);
        let token = table.issue(Scope::Interact, "device".into(), "desktop".into());

        let grant = table.consume(&token).expect("first consumption");
        assert_eq!(grant.scope, Scope::Interact);
        assert_eq!(grant.device_name, "device");
        assert_eq!(grant.platform, "desktop");
        assert_eq!(table.consume(&token).unwrap_err(), "pairing_used");
    }
}
