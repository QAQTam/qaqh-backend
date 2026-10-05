//! Test-only hooks consumed by runtime paths.
//!
//! These switches are intentionally environment based: the daemon is launched
//! as a child process by PTY harnesses, so env is the only configuration
//! channel that reaches the real worker without adding test fields to the wire
//! protocol. The daemon-side registry lives in
//! `crates/qaqh-daemon/src/axum_server/axum_impl/test_hooks.rs`.

/// Whether the deterministic plan-review trigger is enabled.
///
/// When enabled, round 0 of a turn yields a real `PlanReviewRequested`
/// interaction before the provider is called. The subsequent response is
/// handled by the normal `PlanReviewRespond` path.
pub(crate) fn plan_review_enabled() -> bool {
    std::env::var("QAQH_TEST_PLAN_REVIEW")
        .ok()
        .is_some_and(|value| truthy(&value))
}

/// Plan body used by [`plan_review_enabled`].
pub(crate) fn plan_review_content() -> String {
    std::env::var("QAQH_TEST_PLAN_REVIEW_CONTENT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "TUI contract test plan\n\n1. verify plan review rendering\n2. verify approve/reject resume".into())
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_accepts_common_env_forms() {
        for value in ["1", "true", "TRUE", "yes", "on", " on "] {
            assert!(truthy(value), "{value:?}");
        }
        for value in ["", "0", "false", "no", "off", "random"] {
            assert!(!truthy(value), "{value:?}");
        }
    }
}
