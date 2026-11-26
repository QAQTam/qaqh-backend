//! System 域：存活探针、活动面板、版本、停机。
//!
//! 这里的四个能力在骨架期就是**真**的：探针读真实计数，停机真的置停机信号，
//! 所以守卫可以拿它们当"控制面往返"的探针（往返延迟里没有假数据掺水）。

use std::sync::Arc;

use qaqh_ringing_v3::{RpcError, SystemRequest, SystemResponse};

use super::super::state::ServerState;
use super::CallContext;

/// System 门面。
pub(crate) struct SystemFacade {
    state: Arc<ServerState>,
}

impl SystemFacade {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 处理一条 System 请求。
    pub(crate) async fn handle(
        &self,
        context: &CallContext,
        request: SystemRequest,
    ) -> Result<SystemResponse, RpcError> {
        match request {
            SystemRequest::Health => Ok(SystemResponse::Health {
                uptime_ms: self.state.uptime_ms(),
                peers: u32::try_from(self.state.peers.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(u32::MAX),
            }),
            SystemRequest::Activity => {
                let sessions = self.state.sessions();
                let running_tool_calls: u32 = sessions
                    .iter()
                    .map(|session| session.running_tool_calls())
                    .sum();
                Ok(SystemResponse::Activity {
                    active_sessions: u32::try_from(sessions.len()).unwrap_or(u32::MAX),
                    running_tool_calls,
                })
            }
            SystemRequest::Version => Ok(SystemResponse::Version {
                build_id: self.state.config.build_id.clone(),
                protocol_rev: qaqh_ringing_v3::PROTOCOL_REV,
            }),
            SystemRequest::Stop { graceful } => {
                // 停机是管理面动作：已配对设备无权替用户关掉 daemon。
                context.identity.require_owner("System::Stop")?;
                let accepted = self.state.request_shutdown(graceful);
                Ok(SystemResponse::StopAck {
                    accepted,
                    reason: None,
                })
            }
            SystemRequest::StopIfIdle => {
                context.identity.require_owner("System::StopIfIdle")?;
                let busy = self
                    .state
                    .sessions()
                    .iter()
                    .any(|session| session.activity() != qaqh_core_vocab::ActivityState::Idle);
                if busy {
                    return Ok(SystemResponse::StopAck {
                        accepted: false,
                        reason: Some("有会话仍在运行，拒绝停机".to_string()),
                    });
                }
                self.state.request_shutdown(true);
                Ok(SystemResponse::StopAck {
                    accepted: true,
                    reason: None,
                })
            }
        }
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;
    use qaqh_ringing_v3::SystemResponse;

    #[tokio::test]
    async fn health_reports_real_uptime_and_version_reports_build_id() {
        let rig = test_support::rig();
        let context = test_support::owner_context(&rig);
        let health = rig
            .facades
            .system
            .handle(&context, SystemRequest::Health)
            .await
            .expect("Health 必须可用");
        match health {
            SystemResponse::Health { peers, .. } => assert_eq!(peers, 0),
            other => panic!("Health 回执形状不符：{other:?}"),
        }
        let version = rig
            .facades
            .system
            .handle(&context, SystemRequest::Version)
            .await
            .expect("Version 必须可用");
        match version {
            SystemResponse::Version { build_id, .. } => {
                assert_eq!(build_id, rig.state.config.build_id)
            }
            other => panic!("Version 回执形状不符：{other:?}"),
        }
    }

    #[tokio::test]
    async fn stop_requires_owner_scope() {
        let rig = test_support::rig();
        let device = test_support::device_context(&rig);
        let error = rig
            .facades
            .system
            .handle(&device, SystemRequest::Stop { graceful: true })
            .await
            .expect_err("已配对设备无权停机");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::PermissionDenied);
        assert!(!rig.state.shutdown_requested());
    }
}
