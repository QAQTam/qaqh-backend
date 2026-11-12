//! QaqhFFI —— Rust core 的移动端 UniFFI 绑定面（Android 先行，HarmonyOS NAPI 同理）。
//!
//! 设计原则（会话已批准的计划）：
//! - **控制面窄类型**：connect / pair / attach / 流状态（`StreamStatus`）；
//! - **数据面 JSON passthrough**：事件、快照、ack、service 响应按契约 serde
//!   序列化成 JSON 字符串过桥——契约唯一权威在 `qaqh-ringing` / `qaqh-ringing-contract`，
//!   本层不做二次建模，Kotlin 侧模型只消费 JSON；
//! - 传输知识（lease 续租/重协商、退避重连、cursor、SSE 解码、TLS pin）全部
//!   内聚在 `qaqh-client`，FFI 不重复。
//!
//! 编排契约（与 TUI `qaqh-tui-app/src/runtime.rs` 一致）：
//! `connect → attach(seed) → activate_stream(seed)`；收到
//! `StreamStatus::Open` 后调用方应重新 `bootstrap_json` 刷新快照
//! （快照不进流）；`on_reset` 后同样 re-baseline。

// uniffi 生成的 scaffolding 含大量无法标注 SAFETY 的 unsafe 块（extern "C"
// 桥接面），覆盖 workspace 的 clippy deny——本 crate 手写代码无 unsafe。
#![allow(clippy::undocumented_unsafe_blocks)]

uniffi::setup_scaffolding!();

mod error;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

pub use error::CoreError;

use qaqh_client::types::{CommandOptions, ConversationSendMessage};
use qaqh_client::v2_stream::V2StreamStatus;
use qaqh_client::{
    Client, ClientError, ClientHandlers, ClientOptions, QueryRequest, RemoteEndpoint,
    sanitize_session_list,
};
use qaqh_ringing::{CommandTarget, RingingCommand, RingingV2ApprovalRespondRequest};

/// 单次请求级超时（移动端网络不可假设；流式任务不受此限）。
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

// ── 值对象 ────────────────────────────────────────────────────────────────

/// 配对成功结果；`device_token` 明文只此一次，调用方负责立即入库。
#[derive(uniffi::Record, Debug, Clone)]
pub struct PairResult {
    pub device_id: String,
    pub device_token: String,
    pub scope: String,
    pub daemon_version: String,
    pub protocol_version: u32,
}

/// 当前租约的协商结果（ClientV2SessionState 的窄投影）。
#[derive(uniffi::Record, Debug, Clone)]
pub struct LeaseState {
    pub client_instance_id: String,
    pub client_session_id: String,
    pub server_epoch: String,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
}

/// v2 单流状态（`qaqh_client::V2StreamStatus` 的窄映射；`ReconnectReason`
/// 序列化成 `lagged:<n>` / `stream_terminated:<code>` 字符串）。
#[derive(uniffi::Enum, Debug, Clone)]
pub enum StreamStatus {
    Connecting,
    Open {
        server_epoch: String,
        cursor: Option<String>,
    },
    Reconnecting {
        retry_ms: u64,
        reason: Option<String>,
    },
    Closed {
        reason: String,
    },
}

impl From<V2StreamStatus> for StreamStatus {
    fn from(status: V2StreamStatus) -> Self {
        match status {
            V2StreamStatus::Connecting => Self::Connecting,
            V2StreamStatus::Open {
                server_epoch,
                cursor,
            } => Self::Open {
                server_epoch,
                cursor,
            },
            V2StreamStatus::Reconnecting {
                retry_ms,
                reason,
                last_cursor: _,
            } => Self::Reconnecting {
                retry_ms,
                reason: reason.map(|r| match r {
                    qaqh_client::ReconnectReason::Lagged { skipped } => format!("lagged:{skipped}"),
                    qaqh_client::ReconnectReason::StreamTerminated { code } => {
                        format!("stream_terminated:{code}")
                    }
                }),
            },
            V2StreamStatus::Closed { reason } => Self::Closed { reason },
        }
    }
}

/// 外推事件回调（Rust 后台线程 → JVM；Kotlin 侧自行 marshal 到 UI 线程）。
#[uniffi::export(callback_interface)]
pub trait CoreHandlers: Send + Sync {
    /// 单流事件帧；`event_json` 是 `SessionEvent`（tag="type" snake_case）。
    fn on_event(&self, session_id: String, seq: u64, event_json: String);
    /// 服务端 resync_required：调用方应重新 bootstrap 再继续。
    fn on_reset(&self, session_id: String, reason: String);
    /// 单流状态迁移（每 seed 一条）。
    fn on_status(&self, session_id: String, status: StreamStatus);
    /// 字节级存活信号（含 15s keepalive）。
    fn on_liveness(&self);
}

// ── Android 日志 ─────────────────────────────────────────────────────────

#[cfg(target_os = "android")]
fn init_android_logger() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("qaqh-core"),
    );
}

#[cfg(not(target_os = "android"))]
fn init_android_logger() {}

// ── 工具 ────────────────────────────────────────────────────────────────

async fn with_timeout<T>(future: impl Future<Output = Result<T, ClientError>>) -> Result<T, CoreError> {
    match tokio::time::timeout(CALL_TIMEOUT, future).await {
        Ok(result) => result.map_err(CoreError::from),
        Err(_) => Err(CoreError::Transport {
            detail: format!("request timed out after {}s", CALL_TIMEOUT.as_secs()),
        }),
    }
}

fn to_json_string<T: serde::Serialize>(value: &T) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|error| CoreError::Protocol {
        detail: format!("serialize: {error}"),
    })
}

fn parse_json(raw: &str) -> Result<Value, CoreError> {
    serde_json::from_str(raw).map_err(|error| CoreError::Protocol {
        detail: format!("invalid json: {error}"),
    })
}

// ── 顶层入口 ─────────────────────────────────────────────────────────────

/// 手机侧配对：消费一次性 `pairing_token` 换 `device_token`（免鉴权端点）。
#[uniffi::export(async_runtime = "tokio")]
pub async fn pair_device(
    base_url: String,
    pairing_token: String,
    device_name: String,
    platform: String,
    tls_fp: Option<String>,
) -> Result<PairResult, CoreError> {
    init_android_logger();
    let response = qaqh_client::pair_remote(
        &base_url,
        &pairing_token,
        &device_name,
        &platform,
        tls_fp.as_deref(),
    )
    .await?;
    Ok(PairResult {
        device_id: response.device_id,
        device_token: response.device_token,
        scope: response.scope,
        daemon_version: response.daemon_version,
        protocol_version: response.protocol_version,
    })
}

/// 连接 daemon（remote 直连模式）：open 租约 + 起续租/attach 重放后台任务。
///
/// 之后调用方负责 `attach(seed) → activate_stream(seed)` 编排。
#[uniffi::export(async_runtime = "tokio")]
pub async fn connect_client(
    base_url: String,
    device_token: String,
    tls_fp: Option<String>,
    handlers: Box<dyn CoreHandlers>,
) -> Result<Arc<QaqhClient>, CoreError> {
    init_android_logger();
    // Box → Arc：4 个回调闭包共享同一个 foreign 句柄。
    let handlers: Arc<dyn CoreHandlers> = handlers.into();
    let bridge = ClientHandlers {
        on_v2_event: {
            let handlers = Arc::clone(&handlers);
            Arc::new(move |seed, frame| {
                let event_json = serde_json::to_string(&frame.event)
                    .unwrap_or_else(|_| "{}".to_string());
                handlers.on_event(seed, frame.seq, event_json);
            })
        },
        on_v2_reset: {
            let handlers = Arc::clone(&handlers);
            Arc::new(move |seed, reset| handlers.on_reset(seed, reset.reason))
        },
        on_v2_status: {
            let handlers = Arc::clone(&handlers);
            Arc::new(move |seed, status| handlers.on_status(seed, status.into()))
        },
        on_liveness: {
            let handlers = Arc::clone(&handlers);
            Arc::new(move || handlers.on_liveness())
        },
    };
    let client = Client::connect_async(ClientOptions {
        handlers: bridge,
        launch_daemon_if_missing: false,
        remote: Some(RemoteEndpoint {
            base_url,
            token: device_token,
            tls_fp,
        }),
        ..ClientOptions::default()
    })
    .await?;
    Ok(Arc::new(QaqhClient { inner: client }))
}

/// 已连接的 Ringing v2 客户端句柄。
#[derive(uniffi::Object)]
pub struct QaqhClient {
    inner: Client,
}

impl Drop for QaqhClient {
    /// `close()` 幂等（watch send），显式 close 后再 Drop 无害。
    fn drop(&mut self) {
        self.inner.close();
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl QaqhClient {
    // ── 流编排 ──

    /// 当前租约状态（open 协商结果；壳层状态机的 Connected 字段来源）。
    pub async fn lease_state(&self) -> Option<LeaseState> {
        let state = self.inner.v2_session_state().await?;
        Some(LeaseState {
            client_instance_id: state.client_instance_id,
            client_session_id: state.client_session_id,
            server_epoch: state.server_epoch,
            lease_ttl_ms: state.lease_ttl_ms,
            renew_interval_ms: state.renew_interval_ms,
        })
    }

    /// attach 一个 seed（SessionResume 命令；租约重协商后由 client 自动重放）。
    pub async fn attach(&self, session_id: String) -> Result<String, CoreError> {
        let ack = with_timeout(self.inner.attach(&session_id)).await?;
        to_json_string(&ack)
    }

    /// 起一个 seed 的常驻单流（事件走 `CoreHandlers.on_event`）。
    pub async fn activate_stream(&self, session_id: String) -> Result<(), CoreError> {
        self.inner.activate_stream(&session_id).await;
        Ok(())
    }

    pub async fn deactivate_stream(&self, session_id: String) -> Result<(), CoreError> {
        self.inner.deactivate_stream(&session_id).await;
        Ok(())
    }

    /// bootstrap 快照（SessionState JSON，含 `seq` 水位）。流 Open / on_reset
    /// 之后都应重新拉取。
    pub async fn bootstrap_json(&self, session_id: String) -> Result<String, CoreError> {
        let bootstrap = with_timeout(self.inner.bootstrap(&session_id)).await?;
        to_json_string(&bootstrap)
    }

    // ── 查询（QueryRequest 12 变体逐一出口）──

    pub async fn session_list_json(&self) -> Result<String, CoreError> {
        let mut value = with_timeout(self.inner.query(QueryRequest::SessionList)).await?;
        value = sanitize_session_list(value);
        Ok(value.to_string())
    }

    pub async fn session_activity_json(&self) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::SessionActivity))
            .await?
            .to_string())
    }

    pub async fn workspace_list_json(&self) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::WorkspaceList))
            .await?
            .to_string())
    }

    pub async fn config_load_json(&self) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::ConfigLoad))
            .await?
            .to_string())
    }

    pub async fn skills_list_tools_json(&self) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::SkillsListTools))
            .await?
            .to_string())
    }

    pub async fn skills_catalog_json(&self, cwd: Option<String>) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::SkillsCatalog { cwd }))
            .await?
            .to_string())
    }

    pub async fn fs_list_json(&self, path: String) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::FsList { path }))
            .await?
            .to_string())
    }

    pub async fn fs_read_json(&self, path: String, max_bytes: u64) -> Result<String, CoreError> {
        Ok(with_timeout(
            self.inner
                .query(QueryRequest::FsRead { path, max_bytes: Some(max_bytes) }),
        )
            .await?
            .to_string())
    }

    pub async fn session_dashboard_json(&self, session_id: String) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::SessionDashboard { session_id }))
            .await?
            .to_string())
    }

    pub async fn todo_status_json(&self, session_id: String) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::TodoStatus { session_id }))
            .await?
            .to_string())
    }

    pub async fn session_meta_json(&self, session_id: String) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::SessionMeta { session_id }))
            .await?
            .to_string())
    }

    pub async fn plan_read_json(&self, session_id: String) -> Result<String, CoreError> {
        Ok(with_timeout(self.inner.query(QueryRequest::PlanRead { session_id }))
            .await?
            .to_string())
    }

    // ── 命令 ──

    /// 通用命令逃生口：`command_json` 是 `RingingCommand` 的 JSON（tag="type"）。
    /// `session_id = None` → `CommandTarget::Global`。
    pub async fn send_command_json(
        &self,
        session_id: Option<String>,
        command_json: String,
    ) -> Result<String, CoreError> {
        let command: RingingCommand =
            serde_json::from_value(parse_json(&command_json)?).map_err(|error| {
                CoreError::Protocol {
                    detail: format!("unknown command shape: {error}"),
                }
            })?;
        let target = match session_id {
            Some(id) => CommandTarget::Session { session_id: id },
            None => CommandTarget::Global,
        };
        let ack = with_timeout(
            self.inner
                .send_command(target, command, CommandOptions::default()),
        )
        .await?;
        to_json_string(&ack)
    }

    /// `tool.permission.respond`（device 侧 daemon 强制 `trust_folder=false`）。
    pub async fn respond_tool_permission(
        &self,
        session_id: String,
        tool_call_id: String,
        approved: bool,
    ) -> Result<String, CoreError> {
        self.send_command_json(
            Some(session_id),
            to_json_string(&RingingCommand::ToolPermissionRespond {
                tool_call_id,
                approved,
                trust_folder: false,
            })?,
        )
        .await
    }

    /// `interaction.ask.respond`；`answers_json` 是 `Vec<AskAnswer>` 的 JSON。
    pub async fn respond_ask(
        &self,
        session_id: String,
        interaction_id: String,
        answers_json: String,
    ) -> Result<String, CoreError> {
        let answers = serde_json::from_value(parse_json(&answers_json)?).map_err(|error| {
            CoreError::Protocol {
                detail: format!("invalid answers: {error}"),
            }
        })?;
        self.send_command_json(
            Some(session_id),
            to_json_string(&RingingCommand::InteractionAskRespond {
                interaction_id,
                answers,
            })?,
        )
        .await
    }

    /// `driver.claim`；`client_session_id` 由当前租约现取。
    pub async fn driver_claim(
        &self,
        session_id: String,
        stale_holder: Option<String>,
    ) -> Result<String, CoreError> {
        let state = self
            .inner
            .v2_session_state()
            .await
            .ok_or(CoreError::Negotiation {
                detail: "v2 session not open".into(),
            })?;
        self.send_command_json(
            Some(session_id),
            to_json_string(&RingingCommand::DriverClaim {
                client_session_id: state.client_session_id,
                stale_holder,
            })?,
        )
        .await
    }

    /// `driver.release`。
    pub async fn driver_release(&self, session_id: String) -> Result<String, CoreError> {
        let state = self
            .inner
            .v2_session_state()
            .await
            .ok_or(CoreError::Negotiation {
                detail: "v2 session not open".into(),
            })?;
        self.send_command_json(
            Some(session_id),
            to_json_string(&RingingCommand::DriverRelease {
                client_session_id: state.client_session_id,
                expected_epoch: None,
            })?,
        )
        .await
    }

    /// `conversation.send_message`（纯文本便捷面；富内容用 `send_command_json`）。
    pub async fn send_message(&self, session_id: String, text: String) -> Result<String, CoreError> {
        self.send_command_json(
            Some(session_id),
            to_json_string(&RingingCommand::ConversationSendMessage(ConversationSendMessage {
                text,
                images: Vec::new(),
                attachments: None,
                message_id: None,
                input_purpose: Default::default(),
                as_system: false,
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
            }))?,
        )
        .await
    }

    /// `conversation.cancel`。
    pub async fn cancel_turn(
        &self,
        session_id: String,
        turn_id: Option<String>,
    ) -> Result<String, CoreError> {
        self.send_command_json(
            Some(session_id),
            to_json_string(&RingingCommand::ConversationCancel { turn_id })?,
        )
        .await
    }

    // ── device 审批面 ──

    /// 不透明 challenge 列表（`RingingV2DeviceChallenges` JSON）。
    pub async fn device_challenges_json(&self, session_id: String) -> Result<String, CoreError> {
        let challenges = with_timeout(self.inner.device_challenges(&session_id)).await?;
        to_json_string(&challenges)
    }

    /// 审批应答（一次性消费 challenge）→ 命令 ack JSON。
    pub async fn respond_device_approval(
        &self,
        session_id: String,
        challenge_id: String,
        decision: String,
        payload_json: Option<String>,
    ) -> Result<String, CoreError> {
        let payload = match payload_json {
            Some(raw) => parse_json(&raw)?,
            None => Value::Null,
        };
        let ack = with_timeout(
            self.inner
                .respond_device_approval(
                    &session_id,
                    RingingV2ApprovalRespondRequest {
                        challenge_id,
                        decision,
                        payload,
                    },
                ),
        )
        .await?;
        to_json_string(&ack)
    }

    // ── 账号/白名单服务面（siwc.* / dsaccount.* 18 个）──

    pub async fn service_json(
        &self,
        method: String,
        params_json: Option<String>,
    ) -> Result<String, CoreError> {
        let params = match params_json {
            Some(raw) => parse_json(&raw)?,
            None => Value::Null,
        };
        let value = with_timeout(self.inner.service_call(&method, params)).await?;
        Ok(value.to_string())
    }

    // ── content ──

    /// 下载并做 sha256 校验（ContentRef 内含摘要）。
    pub async fn download_content(&self, content_ref: String) -> Result<Vec<u8>, CoreError> {
        let reference = parse_json(&content_ref).and_then(|value| {
            serde_json::from_value(value).map_err(|error| CoreError::Protocol {
                detail: format!("invalid ContentRef: {error}"),
            })
        })?;
        with_timeout(self.inner.download_content(&reference)).await
    }

    /// 上传二进制（如手机相册图）→ `ContentRef` JSON。
    pub async fn upload_content(
        &self,
        session_id: String,
        media_type: String,
        data: Vec<u8>,
    ) -> Result<String, CoreError> {
        let reference =
            with_timeout(self.inner.upload_content(&session_id, &media_type, data)).await?;
        to_json_string(&reference)
    }

    /// 停全部后台任务（续租 + 流）。幂等。（不叫 close：uniffi 生成的
    /// Disposable.close() 是对象销毁语义，避免冲突。）
    pub fn shutdown(&self) {
        self.inner.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_permission_command_json_roundtrips() {
        let raw = r#"{"type":"tool.permission.respond","tool_call_id":"call_01","approved":true}"#;
        let command: RingingCommand =
            serde_json::from_value(parse_json(raw).unwrap()).unwrap();
        assert!(matches!(command, RingingCommand::ToolPermissionRespond { approved: true, .. }));
    }

    #[test]
    fn unknown_command_shape_is_rejected() {
        let raw = r#"{"type":"no.such.command"}"#;
        let parsed = parse_json(raw).unwrap();
        assert!(serde_json::from_value::<RingingCommand>(parsed).is_err());
    }

    #[test]
    fn stream_status_maps_all_variants() {
        assert!(matches!(
            StreamStatus::from(V2StreamStatus::Connecting),
            StreamStatus::Connecting
        ));
        assert!(matches!(
            StreamStatus::from(V2StreamStatus::Open {
                server_epoch: "ep".into(),
                cursor: Some("ep:0".into()),
            }),
            StreamStatus::Open { cursor: Some(c), .. } if c == "ep:0"
        ));
        assert!(matches!(
            StreamStatus::from(V2StreamStatus::Reconnecting {
                retry_ms: 1000,
                reason: Some(qaqh_client::ReconnectReason::StreamTerminated {
                    code: "resync".into()
                }),
                last_cursor: None,
            }),
            StreamStatus::Reconnecting { retry_ms: 1000, reason: Some(r), .. }
                if r == "stream_terminated:resync"
        ));
        assert!(matches!(
            StreamStatus::from(V2StreamStatus::Closed { reason: "x".into() }),
            StreamStatus::Closed { .. }
        ));
    }

    #[test]
    fn client_error_maps_to_judgable_variants() {
        let api = CoreError::from(ClientError::Api {
            status: 403,
            code: "lease_required".into(),
            message: "m".into(),
        });
        assert!(matches!(api, CoreError::Api { code, status: 403, .. } if code == "lease_required"));
        let http = CoreError::from(ClientError::Http { status: 401, path: "/p".into() });
        assert!(matches!(http, CoreError::Http { status: 401, .. }));
    }

    #[tokio::test]
    async fn connect_rejects_bad_base_url_before_network() {
        let result = connect_client(
            "ftp://not-http".into(),
            "token".into(),
            None,
            Box::new(TestHandlers),
        )
        .await;
        assert!(matches!(result, Err(CoreError::Discovery { .. })));
    }

    #[tokio::test]
    async fn pair_device_rejects_bad_base_url_before_network() {
        let result = pair_device("nope".into(), "t".into(), "d".into(), "android".into(), None).await;
        assert!(matches!(result, Err(CoreError::Discovery { .. })));
    }

    /// 什么都不做的回调桩(错误路径测试用,连不上就不会收到事件)。
    struct TestHandlers;
    impl CoreHandlers for TestHandlers {
        fn on_event(&self, _session_id: String, _seq: u64, _event_json: String) {}
        fn on_reset(&self, _session_id: String, _reason: String) {}
        fn on_status(&self, _session_id: String, _status: StreamStatus) {}
        fn on_liveness(&self) {}
    }
}
