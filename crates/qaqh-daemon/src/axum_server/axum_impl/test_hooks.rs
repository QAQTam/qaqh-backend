//! Central registry for daemon-side `QAQH_TEST_*` fault injection.
//!
//! The hooks are read once when `AppState` is assembled. Each one-shot fault
//! consumes an atomic token so a client reconnect after the injected failure
//! sees the normal stream instead of a permanent failure loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qaqh_domain::{RingingChannel, ToolCommand};
use qaqh_ringing::RingingCommand;

/// Scope of an injected SSE termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SseTerminateScope {
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

/// Command predicate for [`CommandAckFault`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandAckTarget {
    InteractionResponses,
    PermissionResponse,
    AskResponse,
    All,
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
    /// Shared by channel and timeline streams: `Scope::Any` matches both, so the
    /// one-shot token must not be consumable once per stream kind.
    sse_terminate_used: AtomicBool,
    timeline_gap: bool,
    timeline_gap_used: AtomicBool,
    session_404_session: Option<String>,
    command_ack: Option<CommandAckFault>,
    command_ack_channel: Option<RingingChannel>,
    command_ack_target: CommandAckTarget,
    interaction_fault: Option<InteractionFault>,
    interaction_fault_used: AtomicBool,
}

impl TestHooks {
    pub(crate) fn disabled() -> Self {
        Self {
            sse_terminate: None,
            sse_terminate_scope: SseTerminateScope::Any,
            sse_terminate_used: AtomicBool::new(false),
            timeline_gap: false,
            timeline_gap_used: AtomicBool::new(false),
            session_404_session: None,
            command_ack: None,
            command_ack_channel: None,
            command_ack_target: CommandAckTarget::InteractionResponses,
            interaction_fault: None,
            interaction_fault_used: AtomicBool::new(false),
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
            .unwrap_or("timeline")
        {
            "any" => SseTerminateScope::Any,
            _ => SseTerminateScope::Timeline,
        };
        let timeline_gap = env_flag("QAQH_TEST_TIMELINE_GAP");
        let session_404_session = env_string("QAQH_TEST_SESSION_404_SEED");
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
        let command_ack_target = match env_string("QAQH_TEST_COMMAND_ACK_COMMAND")
            .as_deref()
            .unwrap_or("interaction")
        {
            "permission_response" => CommandAckTarget::PermissionResponse,
            "ask_response" => CommandAckTarget::AskResponse,
            "all" => CommandAckTarget::All,
            _ => CommandAckTarget::InteractionResponses,
        };
        let interaction_fault = env_string("QAQH_TEST_INTERACTION_FAULT")
            .and_then(|value| parse_interaction_fault(&value));

        Self {
            sse_terminate,
            sse_terminate_scope,
            sse_terminate_used: AtomicBool::new(false),
            timeline_gap,
            timeline_gap_used: AtomicBool::new(false),
            session_404_session,
            command_ack,
            command_ack_channel,
            command_ack_target,
            interaction_fault,
            interaction_fault_used: AtomicBool::new(false),
        }
    }

    /// Shared one-shot consumer for channel and timeline streams.
    fn take_sse_terminate_once(&self) -> Option<SseTerminate> {
        let terminate = self.sse_terminate.clone()?;
        if self.sse_terminate_used.swap(true, Ordering::AcqRel) {
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
        self.take_sse_terminate_once()
    }

    /// Consume the one-shot timeline gap injection.
    pub(crate) fn take_timeline_gap(&self) -> bool {
        self.timeline_gap && !self.timeline_gap_used.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn session_is_404(&self, session_id: &str) -> bool {
        self.session_404_session
            .as_deref()
            .is_some_and(|configured| configured == "*" || configured == session_id)
    }

    pub(crate) fn take_interaction_fault(
        &self,
        command: &RingingCommand,
    ) -> Option<InteractionFault> {
        let fault = self.interaction_fault?;
        let matches = match fault {
            InteractionFault::PermissionDeny | InteractionFault::PermissionHang => matches!(
                command,
                RingingCommand::Tool(ToolCommand::ToolPermissionRespond { .. })
            ),
            InteractionFault::AskDismiss | InteractionFault::AskHang => matches!(
                command,
                RingingCommand::Control(qaqh_domain::ControlCommand::InteractionAskRespond { .. })
            ),
        };
        if !matches {
            return None;
        }
        if matches!(
            fault,
            InteractionFault::PermissionDeny | InteractionFault::AskDismiss
        ) && self.interaction_fault_used.swap(true, Ordering::AcqRel)
        {
            return None;
        }
        Some(fault)
    }

    /// Apply command-ack delay/hang after routing has selected the channel and
    /// parsed the command payload.
    pub(crate) async fn apply_command_ack_fault(
        &self,
        channel: RingingChannel,
        command: &RingingCommand,
    ) {
        let Some(fault) = self.command_ack else {
            return;
        };
        if self
            .command_ack_channel
            .is_some_and(|expected| expected != channel)
            || !self.command_ack_target.matches(command)
        {
            return;
        }
        match fault {
            CommandAckFault::Hang => std::future::pending::<()>().await,
            CommandAckFault::Delay(delay) => tokio::time::sleep(delay).await,
        }
    }
}

impl CommandAckTarget {
    fn matches(self, command: &RingingCommand) -> bool {
        match self {
            Self::InteractionResponses => matches!(
                command,
                RingingCommand::Tool(ToolCommand::ToolPermissionRespond { .. })
                    | RingingCommand::Control(
                        qaqh_domain::ControlCommand::InteractionAskRespond { .. }
                    )
            ),
            Self::PermissionResponse => matches!(
                command,
                RingingCommand::Tool(ToolCommand::ToolPermissionRespond { .. })
            ),
            Self::AskResponse => matches!(
                command,
                RingingCommand::Control(qaqh_domain::ControlCommand::InteractionAskRespond { .. })
            ),
            Self::All => true,
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
    pub(crate) fn for_test_session_404(session_id: &str) -> Self {
        Self {
            session_404_session: Some(session_id.into()),
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

    fn permission_command() -> RingingCommand {
        RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
            tool_call_id: "call-1".into(),
            approved: true,
            trust_folder: false,
        })
    }

    fn ask_command() -> RingingCommand {
        RingingCommand::Control(qaqh_domain::ControlCommand::InteractionAskRespond {
            interaction_id: "ask-1".into(),
            answers: vec![],
        })
    }

    fn conversation_command() -> RingingCommand {
        RingingCommand::Conversation(qaqh_domain::ConversationCommand::ConversationCancel {
            turn_id: None,
        })
    }

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
    fn deny_and_dismiss_faults_are_one_shot_and_type_scoped() {
        let hooks = TestHooks {
            interaction_fault: Some(InteractionFault::PermissionDeny),
            ..TestHooks::disabled()
        };
        assert!(hooks.take_interaction_fault(&ask_command()).is_none());
        assert_eq!(
            hooks.take_interaction_fault(&permission_command()),
            Some(InteractionFault::PermissionDeny)
        );
        assert!(
            hooks
                .take_interaction_fault(&permission_command())
                .is_none()
        );

        let hooks = TestHooks {
            interaction_fault: Some(InteractionFault::AskDismiss),
            ..TestHooks::disabled()
        };
        assert!(
            hooks
                .take_interaction_fault(&permission_command())
                .is_none()
        );
        assert_eq!(
            hooks.take_interaction_fault(&ask_command()),
            Some(InteractionFault::AskDismiss)
        );
        assert!(hooks.take_interaction_fault(&ask_command()).is_none());
    }

    #[test]
    fn hang_faults_remain_persistent_and_type_scoped() {
        let hooks = TestHooks {
            interaction_fault: Some(InteractionFault::PermissionHang),
            ..TestHooks::disabled()
        };
        assert!(hooks.take_interaction_fault(&ask_command()).is_none());
        assert_eq!(
            hooks.take_interaction_fault(&permission_command()),
            Some(InteractionFault::PermissionHang)
        );
        assert_eq!(
            hooks.take_interaction_fault(&permission_command()),
            Some(InteractionFault::PermissionHang)
        );
    }

    #[test]
    fn command_ack_target_defaults_to_interaction_responses() {
        assert!(CommandAckTarget::InteractionResponses.matches(&permission_command()));
        assert!(CommandAckTarget::InteractionResponses.matches(&ask_command()));
        assert!(!CommandAckTarget::InteractionResponses.matches(&conversation_command()));
        assert!(CommandAckTarget::All.matches(&conversation_command()));
    }

    #[test]
    fn session_404_matches_exact_session_or_wildcard() {
        let exact = TestHooks {
            session_404_session: Some("seed-sub".into()),
            ..TestHooks::disabled()
        };
        assert!(exact.session_is_404("seed-sub"));
        assert!(!exact.session_is_404("seed-root"));

        let wildcard = TestHooks {
            session_404_session: Some("*".into()),
            ..TestHooks::disabled()
        };
        assert!(wildcard.session_is_404("anything"));
    }
}
