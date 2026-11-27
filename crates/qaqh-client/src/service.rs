//! 服务面直调的白名单（`POST /ringing/v2/service/{method}` 的客户端侧闸门）。
//!
//! ## 为什么需要这一层
//!
//! [`QueryRequest`](crate::QueryRequest) 是只读服务查询的
//! **封闭枚举**：方法名在 `into_parts` 里逐条映射，好处是拼错即编译错误。代价是
//! 「daemon 已经有、客户端还没加变体」的方法**一个都调不到**——`siwc.*`（8）与
//! `dsaccount.*`（10）整整 18 个账号面方法就是这样：daemon 侧方法表、admin 档位、
//! 实现分支都齐了，客户端库没有出口，于是 TUI / 桌面壳只能自己拼 HTTP 或者干脆做不了。
//!
//! 这里给出一条**有闸门的**通用出口，而不是无限泛型逃生口：
//! 方法名必须落在 [`SERVICE_METHODS`] 里，越界**在客户端就拒**（一个请求都不发，
//! 错误类型是 [`ClientError::ServiceMethodNotAllowed`](crate::ClientError)）。
//!
//! ## 白名单的真源
//!
//! 真源是 `qaqh_siwc::methods::ALL` ∪ `qaqh_dsaccount::methods::ALL`。
//! 本 crate **不**直接依赖那两个 crate（它们会拖进 `reqwest` / `aws-lc-rs` / `qaqh-config`，
//! 而 `qaqh-client` 是壳层的传输层，理应只有 HTTP/SSE 那点东西），
//! 所以这里是字面量——但**不是无人看守的字面量**：
//! `qaqh-runtime` 的 `client_service_whitelist_mirrors_account_methods` 测试
//! 两向遍历（`ALL ⊆ 白名单` 且 `白名单 ⊆ 表`），新增/删除方法时它会先红。
//!
//! ## 登录类方法不要求会话
//!
//! `sign_in.start` 这类方法在「零会话」时也要能调（引导页第一件事就是登录）。
//! 本模块的实现**只**复用已协商好的 client session 头（daemon 侧 `handle_service`
//! 要求它来定位 lease），从不注入 seed、也不要求 `attach`——和桌面端
//! `service_requires_session` 的豁免语义一致。

/// 客户端放行的服务面方法（账号凭据面，全部 `admin_only`）。
///
/// 逐条与 `qaqh_siwc::methods::*` / `qaqh_dsaccount::methods::*` 对齐；
/// 对齐关系由 `qaqh-runtime` 的测试机械保证（见模块文档）。
pub const SERVICE_METHODS: &[&str] = &[
    // ── Sign in with ChatGPT（套餐用量）──
    "siwc.status",         // qaqh_siwc::methods::STATUS
    "siwc.profiles",       // PROFILES
    "siwc.models",         // MODELS
    "siwc.sign_in.start",  // SIGN_IN_START
    "siwc.sign_in.status", // SIGN_IN_STATUS
    "siwc.sign_in.cancel", // SIGN_IN_CANCEL
    "siwc.profile.select", // PROFILE_SELECT
    "siwc.sign_out",       // SIGN_OUT
    // ── DeepSeek 账号（账号余额）──
    "dsaccount.status",         // qaqh_dsaccount::methods::STATUS
    "dsaccount.balance",        // BALANCE
    "dsaccount.bonuses",        // BONUSES
    "dsaccount.sign_in.start",  // SIGN_IN_START
    "dsaccount.sign_in.status", // SIGN_IN_STATUS
    "dsaccount.sign_in.cancel", // SIGN_IN_CANCEL
    "dsaccount.sign_out",       // SIGN_OUT
    "dsaccount.bonus.ack",      // ACK_BONUS
    "dsaccount.models",         // MODELS
    "dsaccount.verify",         // VERIFY
];

/// 用户 shell 服务面三条（ADR-0028 切片 3 的 daemon 方法表，切片 5 的客户端出口）。
///
/// **刻意不并进 [`SERVICE_METHODS`]**：那张表的真源是
/// `qaqh_siwc::methods::ALL ∪ qaqh_dsaccount::methods::ALL`，并且 `qaqh-runtime`
/// 的 `client_service_whitelist_mirrors_account_methods` 逐条断言「白名单里的每条
/// 都是 **admin_only** 的账号面方法」。把 `shell.*` 塞进去不是多加三条名字，是把
/// 那条闸门的语义改成「账号面 or 别的什么」——正好是它想防的东西。
///
/// 分成两张表，各自有自己的闸：这张由
/// `client_shell_method_table_mirrors_the_daemon_shell_surface` 双向锁住
/// （daemon 表里 `shell.` 开头的集合 ⊆ 这张表，且这张表的每条都在 daemon 表里）。
pub const SHELL_METHODS: &[&str] = &[
    "shell.open",  // service_methods::METHODS —— 开一条 shell（owner 由 daemon 反查）
    "shell.close", // 收走一条 shell（终止原因的唯一入口）
    "shell.list",  // 本身份已开的 shell（几何真值也从这里读）
];

/// 沙箱启动自检服务面两条（ADR-0040：前端启动动画轮询沙箱自检进度与修复建议）。
pub const SANDBOX_METHODS: &[&str] = &["sandbox.preflight.start", "sandbox.preflight.status"];

/// gitx worktree 写面四条（ADR-0041：worktree 隔离的变更操作）。
///
/// **刻意不并进 [`SERVICE_METHODS`]**：同 [`SHELL_METHODS`] 的理由——那张表的
/// 真源是账号面 methods 模块，并有 `client_service_whitelist_mirrors_account_methods`
/// 的 admin_only 语义闸。读面两条（`gitx.snapshot`/`gitx.worktree_list`）走
/// [`crate::QueryRequest`] 封闭枚举；这四条写面经 `service_call` 透传，
/// daemon 侧由 `qaqh-runtime` 的 TYPED_METHODS 形状闸与 gitx 镜像测试双向锁住。
pub const GITX_METHODS: &[&str] = &[
    "gitx.worktree_create",
    "gitx.worktree_remove",
    "gitx.worktree_bind",
    "gitx.worktree_unbind",
];

/// 方法名是否在放行范围内（`service_call` 与调用方的预检共用同一条判定）。
///
/// 两张表共用一个判定入口，是为了让「客户端放行了什么」这个问题只有**一处**回答
/// ——napi 的 `service_methods()`、主进程 IPC 闸门、`Client::service_call` 的本地
/// 拒绝，都从这里取，不再各写一份并集。
pub fn is_allowed(method: &str) -> bool {
    SERVICE_METHODS.contains(&method)
        || SHELL_METHODS.contains(&method)
        || SANDBOX_METHODS.contains(&method)
        || GITX_METHODS.contains(&method)
}

/// 把非 2xx 响应体翻成结构化错误。
///
/// daemon 服务面的失败体统一是 `{ "code": …, "message": … }`
/// （`service_methods::error_response`）：`code` 让调用方能把「方法不存在」
/// （`unknown_method`）、「缺 lease」（`lease_required`）与「查询/执行失败」
/// （`query_failed` / `action_failed`）分开。
/// 解析不出来就退回 [`ClientError::Http`](crate::ClientError)，绝不假装成功。
pub(crate) fn response_error(status: u16, path: &str, body: &str) -> crate::ClientError {
    #[derive(serde::Deserialize)]
    struct Wire {
        code: String,
        #[serde(default)]
        message: String,
    }

    match serde_json::from_str::<Wire>(body) {
        Ok(wire) => crate::ClientError::Api {
            status,
            code: wire.code,
            message: wire.message,
        },
        Err(_) => crate::ClientError::Http {
            status,
            path: path.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// 18 条：8 个 `siwc.*` + 10 个 `dsaccount.*`。条数断言是**有意**的：
    /// 白名单缩水会立刻红，而不是等到某个壳层调不到时才发现。
    #[test]
    fn whitelist_covers_the_whole_account_surface() {
        assert_eq!(SERVICE_METHODS.len(), 18, "{SERVICE_METHODS:?}");
        assert_eq!(
            SERVICE_METHODS
                .iter()
                .filter(|method| method.starts_with("siwc."))
                .count(),
            8
        );
        assert_eq!(
            SERVICE_METHODS
                .iter()
                .filter(|method| method.starts_with("dsaccount."))
                .count(),
            10
        );
        // 无重复（重复只会让 `contains` 看起来更宽）。
        let mut sorted = SERVICE_METHODS.to_vec();
        let total = sorted.len();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), total, "白名单有重复条目");
    }

    #[test]
    fn non_whitelisted_methods_are_rejected_locally() {
        assert!(is_allowed("siwc.sign_in.start"));
        assert!(is_allowed(qaqh_dsaccount_verify()));
        // 表里存在、但**刻意**没进白名单的方法（进程级诊断只给 daemon 自己用）。
        assert!(!is_allowed("diagnostics.memory.snapshot"));
        // 会话写操作走唯一 RingingCommand 入口，不属服务面。
        assert!(!is_allowed("session.resume"));
        assert!(!is_allowed(""));
    }

    #[test]
    fn shell_methods_are_allowed_and_input_is_not_an_rpc() {
        for method in SHELL_METHODS {
            assert!(
                is_allowed(method),
                "{method} 应被放行（桌面壳的 shell 服务面）"
            );
        }
        assert_eq!(SHELL_METHODS.len(), 3, "{SHELL_METHODS:?}");
        let mut sorted = SHELL_METHODS.to_vec();
        let total = sorted.len();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), total, "shell 表有重复条目");
        // `shell.write` / `shell.output` 是候选 A（长轮询）的形状：放行它们就等于
        // 给同一个终端开第二套传输（ADR-0028 §被否选项第 5 条点名的「静并存」）。
        assert!(!is_allowed("shell.write"));
        assert!(!is_allowed("shell.output"));
    }

    fn qaqh_dsaccount_verify() -> &'static str {
        "dsaccount.verify"
    }

    #[test]
    fn structured_error_keeps_the_daemon_code() {
        let error = response_error(
            404,
            "/ringing/v2/service/x.y",
            r#"{"code":"unknown_method","message":"unknown service method"}"#,
        );
        assert_eq!(error.code(), Some("unknown_method"));
        assert!(matches!(error, crate::ClientError::Api { status: 404, .. }));
    }

    #[test]
    fn non_json_error_body_falls_back_to_http() {
        let error = response_error(502, "/ringing/v2/service/x.y", "<html>bad gateway</html>");
        assert_eq!(error.code(), None);
        assert!(matches!(
            error,
            crate::ClientError::Http { status: 502, .. }
        ));
    }

    #[test]
    fn params_object_drops_absent_keys() {
        /// 参数构造函数：把 `None` 的键整条省掉（服务面按「键缺失」区分「没给」与「给空」）。
        fn params_object(
            entries: impl IntoIterator<Item = (&'static str, Option<Value>)>,
        ) -> Value {
            let mut object = serde_json::Map::new();
            for (key, value) in entries {
                if let Some(value) = value {
                    object.insert(key.to_string(), value);
                }
            }
            Value::Object(object)
        }

        let params = params_object([("profile_id", Some(Value::from("p1"))), ("label", None)]);
        assert_eq!(params, serde_json::json!({ "profile_id": "p1" }));
    }
}
