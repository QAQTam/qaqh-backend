use std::sync::atomic::Ordering;

use qaqh_ringing_v3::{RpcError, SystemRequest, SystemResponse};

use super::RealBackend;
use crate::v3_server::facade::CallContext;

pub(crate) async fn handle(
    backend: &RealBackend,
    context: &CallContext,
    request: SystemRequest,
) -> Result<SystemResponse, RpcError> {
    match request {
        SystemRequest::Health => Ok(SystemResponse::Health {
            uptime_ms: backend.state.uptime_ms(),
            peers: u32::try_from(backend.state.peers.load(Ordering::Relaxed)).unwrap_or(u32::MAX),
        }),
        SystemRequest::Activity => {
            let service = backend.service.clone();
            let (_, sessions) =
                RealBackend::blocking(move || Ok::<_, String>(service.activity_snapshot())).await?;
            let active_sessions = sessions
                .iter()
                .filter(|session| {
                    !matches!(
                        session.state,
                        qaqh_ringing_contract::state::SessionActivityState::Idle
                    )
                })
                .count();
            let running_tool_calls = backend
                .state
                .sessions()
                .iter()
                .map(|session| session.running_tool_calls())
                .sum();
            Ok(SystemResponse::Activity {
                active_sessions: u32::try_from(active_sessions).unwrap_or(u32::MAX),
                running_tool_calls,
            })
        }
        SystemRequest::Version => Ok(SystemResponse::Version {
            build_id: backend.state.config.build_id.clone(),
            protocol_rev: qaqh_ringing_v3::PROTOCOL_REV,
        }),
        SystemRequest::Stop { graceful } => {
            context.identity.require_owner("System::Stop")?;
            Ok(SystemResponse::StopAck {
                accepted: true,
                reason: Some(format!("graceful={graceful}")),
            })
        }
        SystemRequest::StopIfIdle => {
            context.identity.require_owner("System::StopIfIdle")?;
            let service = backend.service.clone();
            let busy =
                RealBackend::blocking(move || Ok::<_, String>(service.has_active_work())).await?;
            if busy {
                return Ok(SystemResponse::StopAck {
                    accepted: false,
                    reason: Some("有会话仍在运行，拒绝停机".to_string()),
                });
            }
            Ok(SystemResponse::StopAck {
                accepted: true,
                reason: None,
            })
        }
    }
}
