//! PKCE（RFC 7636，S256）与 `state` 的一次性事务量。
//!
//! 两个值都由 [`crate::util::random_transaction_value`] 生成（32 字节熵）。
//! `code_verifier` **不出进程**：它只出现在换码与取消两个请求体里，从不进 UI
//! 投影、不进日志。

use ring::digest::{SHA256, digest};

use crate::util::{base64url_encode, constant_time_eq, random_transaction_value};

/// 一次授权尝试需要的两个秘密值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    verifier: String,
    challenge: String,
    state: String,
}

impl Pkce {
    /// 生成一对新的 verifier/challenge 与配套 `state`。
    pub fn new() -> Self {
        let verifier = random_transaction_value();
        Self {
            challenge: challenge_for(&verifier),
            verifier,
            state: random_transaction_value(),
        }
    }

    /// `code_verifier`（换码与取消时回传）。
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// `code_challenge`（`auth_init` 时回传，method 固定 `S256`）。
    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// `state`（`auth_init` 时回传；服务端记住它，回调时原样带回）。
    pub fn state(&self) -> &str {
        &self.state
    }

    /// 校验回调里的 `state` 与本次尝试一致。不一致即**丢弃**这次回调
    /// （可能是别的窗口/别的一次尝试串进来了），不做任何换码。
    pub fn state_matches(&self, received: &str) -> bool {
        constant_time_eq(self.state.as_bytes(), received.as_bytes())
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

/// `code_challenge = BASE64URL(SHA256(ASCII(code_verifier)))`。
pub fn challenge_for(verifier: &str) -> String {
    base64url_encode(digest(&SHA256, verifier.as_bytes()).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_the_sha256_of_the_verifier() {
        // RFC 7636 附录 B 的示例对：verifier 的 S256 challenge。
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn fresh_attempts_are_distinct_and_state_comparison_is_exact() {
        let first = Pkce::new();
        let second = Pkce::new();
        assert_ne!(first.verifier(), second.verifier());
        assert_ne!(first.state(), second.state());
        assert_eq!(first.verifier().len(), 43);
        assert_eq!(first.challenge().len(), 43);
        assert!(first.state_matches(first.state()));
        assert!(!first.state_matches(second.state()));
        assert!(!first.state_matches(""));
    }
}
