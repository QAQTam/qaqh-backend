//! Shell 域门面：**独立并发句柄表**（`ADR-0042` 铁律 4）。
//!
//! ## 为什么不碰中央锁
//!
//! PTY 刷屏是整条连接上最热的数据面：一次 `cat` 就能把几 MB 字节推到屏幕。
//! 如果每次产出都要先过会话表/registry 锁，任何一个慢操作都能把终端卡住。
//! 所以 shell 句柄有**自己的表**（[`ShellTable`]），开门/写字节/调整/关闭
//! 全程只碰这把锁；会话表一次都不碰。
//!
//! ## 断联不杀进程
//!
//! `shell` 的生命周期与连接无关：连接断了只是**附着点没了**（sink 置空），
//! 句柄、输出水位、回放历史全在。另一个客户端可以从 `output_offset`
//! 续读（`from_offset`），看到的字节与断联前连续。这就是守卫③的物理基础。

use std::sync::Arc;

use bytes::Bytes;
use qaqh_core_vocab::SessionId;
use qaqh_ringing_v3::{RpcError, ShellRequest, ShellResponse};

use super::super::error::{denied, not_found};
use super::super::state::{ServerState, ShellHandle};
use super::CallContext;

/// Shell 门面。
pub(crate) struct ShellFacade {
    state: Arc<ServerState>,
}

impl ShellFacade {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 取 shell 并校验：访问外层看设备 -> 会话授权，内层看本会话是否可见。
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
        let session = self
            .state
            .session(&handle.session_id)
            .ok_or_else(|| not_found("shell 所属会话已不存在".to_string()))?;
        if !session.can_access(&context.identity.device) {
            return Err(denied(format!("设备无权访问 shell `{shell_id}`")));
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
            ShellRequest::Open { cols, rows, cwd: _ } => {
                // 会话 id 从哪来？骨架期用"设备默认会话"：没有会话时拒绝，
                // 因为 shell 必须挂在某个会话的授权根上（不能凭空造一个）。
                let session = self.default_session(context)?;
                let handle = self.state.shells.open(
                    &session.id,
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
                // 骨架期的"进程"就是 echo：写进去什么，原样吐回 PTY 流。
                // `v4.7` 把这里换成真 PTY 写端，外部的字节语义完全不变。
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
            ShellRequest::List => {
                let session = self.default_session(context)?;
                Ok(ShellResponse::Shells(self.state.shells.list(&session.id)))
            }
        }
    }

    /// 骨架期的"当前会话"（`List` 与调用方可见的第一个会话）。
    fn default_session(
        &self,
        context: &CallContext,
    ) -> Result<Arc<super::super::state::SessionRuntime>, RpcError> {
        let session = self
            .state
            .sessions()
            .into_iter()
            .find(|session| session.can_access(&context.identity.device))
            .ok_or_else(|| {
                not_found("当前设备还没有可挂载的会话（先建会话再开 shell）".to_string())
            })?;
        Ok(session)
    }
}

/// 取某会话的 shell 列表（`connection` 层给 `ShellPty` 流校验用）。
pub(crate) fn session_shell_ids(state: &ServerState, session_id: &SessionId) -> Vec<String> {
    state
        .shells
        .list(session_id)
        .into_iter()
        .map(|view| view.shell_id)
        .collect()
}

/// 原样字节（骨架期 echo 的显式标记，避免被误读成"加工过"的文本）。
pub(crate) fn raw_bytes(bytes: Bytes) -> Bytes {
    bytes
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;

    #[tokio::test]
    async fn shell_open_write_and_list_touch_only_the_shell_table() {
        let rig = test_support::rig();
        let context = test_support::owner_context(&rig);
        let session = rig.state.create_session(&context.identity.device);
        let opened = rig
            .facades
            .shell
            .handle(
                &context,
                ShellRequest::Open {
                    cols: 80,
                    rows: 24,
                    cwd: None,
                },
            )
            .await
            .expect("开 shell");
        let shell_id = match opened {
            ShellResponse::Opened(view) => view.shell_id,
            other => panic!("形状不符：{other:?}"),
        };
        let written = rig
            .facades
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
            .facades
            .shell
            .handle(&context, ShellRequest::List)
            .await
            .expect("列表");
        match listed {
            ShellResponse::Shells(shells) => {
                assert_eq!(shells.len(), 1);
                // 水位是**权威字节数**：写进去 3 字节，回显也是 3 字节。
                assert_eq!(shells[0].output_offset, 3);
            }
            other => panic!("形状不符：{other:?}"),
        }
        assert_eq!(rig.state.shells.len(), 1);
        assert_eq!(session.id, rig.state.shells.get(&shell_id).expect("句柄").session_id);
    }

    #[tokio::test]
    async fn shell_access_is_scoped_to_the_device() {
        let rig = test_support::rig();
        let owner = test_support::owner_context(&rig);
        let device = test_support::device_context(&rig);
        rig.state.create_session(&owner.identity.device);
        let opened = rig
            .facades
            .shell
            .handle(
                &owner,
                ShellRequest::Open {
                    cols: 80,
                    rows: 24,
                    cwd: None,
                },
            )
            .await
            .expect("开 shell");
        let shell_id = match opened {
            ShellResponse::Opened(view) => view.shell_id,
            other => panic!("形状不符：{other:?}"),
        };
        let error = rig
            .facades
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
