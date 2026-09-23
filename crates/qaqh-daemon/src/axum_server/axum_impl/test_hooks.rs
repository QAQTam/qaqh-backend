//! Central registry for daemon-side `QAQH_TEST_*` fault injection.
//!
//! The hooks are read once when `AppState` is assembled. Each one-shot fault
//! consumes an atomic token so a client reconnect after the injected failure
//! sees the normal stream instead of a permanent failure loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qaqh_domain::RingingChannel;

/// Scope of an injected SSE termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseTerminateScope {
    Channel,
    Timeline,
    Any,
}

/// One injected `ringing.stream_terminated` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseTerminate {
    pub code: String,
    pub skipped: Option<u64>,
}

/// Injected delay/hang for command acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandAckFault {
    Hang,
    Delay(Duration),
}

/// Injected outcome for permission/ask response commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionFault {
    PermissionDeny,
    PermissionHang,
    AskDismiss,
    AskHang,
}

#[derive(Debug)]
pub(crate) struct TestHooks {
    sse_terminate: Option<SseTerminate>,
    sse_terminate_scope: SseTerminateScope,
    sse_terminate_channel: Option<RingingChannel>,
    channel_terminate_used: AtomicBool,
    timeline_terminate_used: AtomicBool,
    timeline_gap: bool,
    timeline_gap_used: AtomicBool,
    session_404_seed: Option<String>,
    command_ack: Option<CommandAckFault>,
    command_ack_channel: Option<RingingChannel>,
    interaction_fault: Option<InteractionFault>,
}

impl TestHooks {
    pub(crate) fn disabled() -> Self {
        Self {
            sse_terminate: None,
            sse_terminate_scope: SseTerminateScope::Any,
            sse_terminate_channel: None,
            channel_terminate_used: AtomicBool::new(false),
            timeline_terminate_used: AtomicBool::new(false),
            timeline_gap: false,
            timeline_gap_used: AtomicBool::new(false),
            session_404_seed: None,
            command_ack: None,
            command_ack_channel: None,
            interaction_fault: None,
        }
    }

    pub(crate) fn from_env() -> Self {
        let sse_terminate = env_string("QAQH_TEST_SSE_TERMINATE").map(|code| SseTerminate {
            skipped: if code == "lagged" {
                Some(
                    env_string("QAQH_TEST_SSE_TERMINATE_SKIPPED")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(7),
                )
            } else {
                None
            },
            code,
        });
        let sse_terminate_scope = match env_string("QAQH_TEST_SSE_TERMINATE_SCOPE")
            .as_deref()
            .unwrap_or("channel")
        {
            "timeline" => SseTerminateScope::Timeline,
            "any" => SseTerminateScope::Any,
            _ => SseTerminateScope::Channel,
        };
        let sse_terminate_channel = env_string("QAQH_TEST_SSE_TERMINATE_CHANNEL")
            .and_then(|value| parse_channel_name(&value));
        let timeline_gap = env_flag("QAQH_TEST_TIMELINE_GAP");
        let session_404_seed = env_string("QAQH_TEST_SESSION_404_SEED");
        let command_ack = env_string("QAQH_TEST_COMMAND_ACK").and_then(|value| {
            if value.eq_ignore_ascii_case("hang") {
                Some(CommandAckFault::Hang)
            } else {
                value
                    .parse::<u64>()
                    .ok()
                    .map(|ms| CommandAckFault::Delay(Duration::from_millis(ms)))
            }
        });
        let command_ack_channel = env_string("QAQH_TEST_COMMAND_ACK_CHANNEL")
            .and_then(|value| parse_channel_name(&value));
        let interaction_fault = env_string("QAQH_TEST_INTERACTION_FAULT")
            .and_then(|value| parse_interaction_fault(&value));

        Self {
            sse_terminate,
            sse_terminate_scope,
            sse_terminate_channel,
            channel_terminate_used: AtomicBool::new(false),
            timeline_terminate_used: AtomicBool::new(false),
            timeline_gap,
            timeline_gap_used: AtomicBool::new(false),
            session_404_seed,
            command_ack,
            command_ack_channel,
            interaction_fault,
        }
    }

    /// Consume the channel-stream termination token, if configured for this
    /// channel. Returns `None` when the hook is absent, scoped elsewhere, or
    /// already consumed.
    pub(crate) fn take_channel_terminate(&self, channel: RingingChannel) -> Option<SseTerminate> {
        if !matches!(
            self.sse_terminate_scope,
            SseTerminateScope::Channel | SseTerminateScope::Any
        ) || self
            .sse_terminate_channel
            .is_some_and(|expected| expected != channel)
        {
            return None;
        }
        let terminate = self.sse_terminate.clone()?;
        if self.channel_terminate_used.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(terminate)
    }

    /// Consume the timeline-stream termination token.
    pub(crate) fn take_timeline_terminate(&self) -> Option<SseTerminate> {
        if !matches!(
            self.sse_terminate_scope,
            SseTerminateScope::Timeline | SseTerminateScope::Any
        ) {
            return None;
        }
        let terminate = self.sse_terminate.clone()?;
        if self.timeline_terminate_used.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(terminate)
    }

    /// Consume the one-shot timeline gap injection.
    pub(crate) fn take_timeline_gap(&self) -> bool {
        self.timeline_gap && !self.timeline_gap_used.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn session_is_404(&self, seed: &str) -> bool {
        self.session_404_seed
            .as_deref()
            .is_some_and(|configured| configured == "*" || configured == seed)
    }

    pub(crate) fn interaction_fault(&self) -> Option<InteractionFault> {
        self.interaction_fault
    }

    /// Apply command-ack delay/hang after routing has selected the channel.
    pub(crate) async fn apply_command_ack_fault(&self, channel: RingingChannel) {
        let Some(fault) = self.command_ack else {
            return;
        };
        if self
            .command_ack_channel
            .is_some_and(|expected| expected != channel)
        {
            return;
        }
        match fault {
            CommandAckFault::Hang => std::future::pending::<()>().await,
            CommandAckFault::Delay(delay) => tokio::time::sleep(delay).await,
        }
    }
}

impl Default for TestHooks {
    fn default() -> Self {
        Self::disabled()
    }
}

#[cfg(test)]
impl TestHooks {
    pub(crate) fn for_test_sse_terminate(
        code: &str,
        scope: SseTerminateScope,
        channel: Option<RingingChannel>,
    ) -> Self {
        Self {
            sse_terminate: Some(SseTerminate {
                code: code.into(),
                skipped: (code == "lagged").then_some(7),
            }),
            sse_terminate_scope: scope,
            sse_terminate_channel: channel,
            ..Self::disabled()
        }
    }

    pub(crate) fn for_test_session_404(seed: &str) -> Self {
        Self {
            session_404_seed: Some(seed.into()),
            ..Self::disabled()
        }
    }

    pub(crate) fn for_test_timeline_gap() -> Self {
        Self {
            timeline_gap: true,
            ..Self::disabled()
        }
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_flag(name: &str) -> bool {
    env_string(name).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn parse_channel_name(value: &str) -> Option<RingingChannel> {
    match value {
        "control" => Some(RingingChannel::Control),
        "conversation" => Some(RingingChannel::Conversation),
        "tool" => Some(RingingChannel::Tool),
        _ => None,
    }
}

fn parse_interaction_fault(value: &str) -> Option<InteractionFault> {
    match value {
        "permission-deny" => Some(InteractionFault::PermissionDeny),
        "permission-hang" => Some(InteractionFault::PermissionHang),
        "ask-dismiss" => Some(InteractionFault::AskDismiss),
        "ask-hang" => Some(InteractionFault::AskHang),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_name_parser_is_closed() {
        assert_eq!(parse_channel_name("control"), Some(RingingChannel::Control));
        assert_eq!(
            parse_channel_name("conversation"),
            Some(RingingChannel::Conversation)
        );
        assert_eq!(parse_channel_name("tool"), Some(RingingChannel::Tool));
        assert_eq!(parse_channel_name("timeline"), None);
        assert_eq!(parse_channel_name("*"), None);
    }

    #[test]
    fn interaction_fault_names_are_closed() {
        assert_eq!(
            parse_interaction_fault("permission-deny"),
            Some(InteractionFault::PermissionDeny)
        );
        assert_eq!(
            parse_interaction_fault("permission-hang"),
            Some(InteractionFault::PermissionHang)
        );
        assert_eq!(
            parse_interaction_fault("ask-dismiss"),
            Some(InteractionFault::AskDismiss)
        );
        assert_eq!(
            parse_interaction_fault("ask-hang"),
            Some(InteractionFault::AskHang)
        );
        assert_eq!(parse_interaction_fault("deny"), None);
        assert_eq!(parse_interaction_fault("permission-disconnect"), None);
    }

    #[test]
    fn session_404_matches_exact_seed_or_wildcard() {
        let exact = TestHooks {
            session_404_seed: Some("seed-sub".into()),
            ..TestHooks::disabled()
        };
        assert!(exact.session_is_404("seed-sub"));
        assert!(!exact.session_is_404("seed-root"));

        let wildcard = TestHooks {
            session_404_seed: Some("*".into()),
            ..TestHooks::disabled()
        };
        assert!(wildcard.session_is_404("anything"));
    }
}
