//! Shell 域门面：**独立并发句柄表**（`ADR-0042` 铁律 4/6）。

use std::sync::Arc;

use qaqh_core_vocab::SessionId;
use qaqh_ringing_v3::{RpcError, ShellRequest, ShellResponse};

use super::super::error::{denied, not_found};
use super::super::shell_table::ShellHandle;
use super::super::state::ServerState;
use super::CallContext;

/// Shell 门面骨架实现。
pub(crate) struct SkeletonShell {
    state: Arc<ServerState>,
}

impl SkeletonShell {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    fn require_session_access(
        &self,
        context: &CallContext,
        session_id: &SessionId,
    ) -> Result<(), RpcError> {
        let session = self
            .state
            .session(session_id)
            .ok_or_else(|| not_found(format!("没有会话 `{session_id}`")))?;
        if !session.can_access(&context.identity.device) {
            return Err(denied(format!("设备无权访问会话 `{session_id}`")));
        }
        Ok(())
    }

    fn shell_for(
        &self,
        context: &CallContext,
        shell_id: &str,
    ) -> Result<Arc<ShellHandle>, RpcError> {
        let handle = self
            .state
            .shells
            .get(shell_id)
            .ok_or_else(|| not_found(format!("没有 shell `{shell_id}`")))?;
        if let Some(session_id) = &handle.session_id {
            self.require_session_access(context, session_id)?;
        } else if handle.owner != context.identity.device {
            return Err(denied(format!("设备无权访问全局 shell `{shell_id}`")));
        }
        Ok(handle)
    }

    /// 处理一条 Shell 请求。
    pub(crate) async fn handle(
        &self,
        context: &CallContext,
        request: ShellRequest,
    ) -> Result<ShellResponse, RpcError> {
        match request {
            ShellRequest::Open {
                session_id,
                cols,
                rows,
                cwd: _,
                shell: _,
            } => {
                if let Some(sid) = &session_id {
                    self.require_session_access(context, sid)?;
                }
                let handle = self.state.shells.open(
                    session_id.as_ref(),
                    &context.identity.device,
                    cols,
                    rows,
                    &self.state.config,
                );
                Ok(ShellResponse::Opened(handle.view()))
            }
            ShellRequest::Write { shell_id, bytes } => {
                let handle = self.shell_for(context, &shell_id)?;
                if handle.exited() {
                    return Err(not_found(format!("shell `{shell_id}` 已结束")));
                }
                let written = bytes.len() as u64;
                handle.push_output(bytes);
                Ok(ShellResponse::Written {
                    bytes_written: written,
                })
            }
            ShellRequest::Resize {
                shell_id,
                cols,
                rows,
            } => {
                let handle = self.shell_for(context, &shell_id)?;
                handle.resize(cols, rows);
                Ok(ShellResponse::Resized)
            }
            ShellRequest::Close { shell_id } => {
                let handle = self.shell_for(context, &shell_id)?;
                if !self.state.shells.close(&handle.id) {
                    return Err(not_found(format!("shell `{shell_id}` 已不存在")));
                }
                Ok(ShellResponse::Closed)
            }
            ShellRequest::List { session_id } => {
                if let Some(sid) = &session_id {
                    self.require_session_access(context, sid)?;
                }
                Ok(ShellResponse::Shells(
                    self.state
                        .shells
                        .list(&context.identity.device, session_id.as_ref()),
                ))
            }
        }
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;
    use bytes::Bytes;

    #[tokio::test]
    async fn shell_open_write_and_list_touch_only_the_shell_table() {
        let rig = test_support::rig();
        let context = test_support::owner_context(&rig);
        let session = rig.state.create_session(&context.identity.device);
        let opened = rig
            .backend
            .shell
            .handle(
                &context,
                ShellRequest::Open {
                    session_id: Some(session.id.clone()),
                    cols: 80,
                    rows: 24,
                    cwd: None,
                    shell: None,
                },
            )
            .await
            .expect("开 shell");
        let shell_id = match opened {
            ShellResponse::Opened(view) => view.shell_id,
            other => panic!("形状不符：{other:?}"),
        };
        let written = rig
            .backend
            .shell
            .handle(
                &context,
                ShellRequest::Write {
                    shell_id: shell_id.clone(),
                    bytes: Bytes::from_static(b"ls\r"),
                },
            )
            .await
            .expect("写入");
        match written {
            ShellResponse::Written { bytes_written } => assert_eq!(bytes_written, 3),
            other => panic!("形状不符：{other:?}"),
        }
        let listed = rig
            .backend
            .shell
            .handle(
                &context,
                ShellRequest::List {
                    session_id: Some(session.id.clone()),
                },
            )
            .await
            .expect("列表");
        match listed {
            ShellResponse::Shells(shells) => {
                assert_eq!(shells.len(), 1);
                assert_eq!(shells[0].output_offset, 3);
            }
            other => panic!("形状不符：{other:?}"),
        }
        assert_eq!(rig.state.shells.len(), 1);
        assert_eq!(
            Some(session.id.clone()),
            rig.state.shells.get(&shell_id).expect("句柄").session_id
        );
    }

    #[tokio::test]
    async fn shell_access_is_scoped_to_the_device() {
        let rig = test_support::rig();
        let owner = test_support::owner_context(&rig);
        let device = test_support::device_context(&rig);
        let session = rig.state.create_session(&owner.identity.device);
        let opened = rig
            .backend
            .shell
            .handle(
                &owner,
                ShellRequest::Open {
                    session_id: Some(session.id.clone()),
                    cols: 80,
                    rows: 24,
                    cwd: None,
                    shell: None,
                },
            )
            .await
            .expect("开 shell");
        let shell_id = match opened {
            ShellResponse::Opened(view) => view.shell_id,
            other => panic!("形状不符：{other:?}"),
        };
        let error = rig
            .backend
            .shell
            .handle(
                &device,
                ShellRequest::Write {
                    shell_id,
                    bytes: Bytes::from_static(b"whoami\r"),
                },
            )
            .await
            .expect_err("跨设备写 shell 必须拒绝");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::PermissionDenied);
    }
}
