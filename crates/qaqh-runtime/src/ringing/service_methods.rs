//! Ringing 服务面方法表（`POST /ringing/v2/service/{method}` 的单一权威清单）。
//!
//! 旧 `/queries/{name}`（闭表白名单）与 `/actions/{name}`（前缀 allowlist）
//! 双端点已合并于此，其 slash/dot 双别名容忍一并拆除：单一规范形态
//! `module.method`，一个方法一个条目，`Read` = 无副作用查询，`Write` = 变更操作。
//!
//! 会话生命周期（`session.new`/`session.resume`/`skills.activate`/`todo.cancel`/
//! `plan.action` 等命令语义方法）刻意不在表中——会话命令只走三频道
//! command envelope（开发标准 N5），服务面仅承载非会话 RPC。

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

pub mod accounts;

use crate::QaqhService;

/// 方法类别：决定错误码形状（`query_failed` / `action_failed`），
/// 也是对调用方"只读 / 变更"的契约声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodInfo {
    pub kind: MethodKind,
    /// 要求会话参数并做 lease 归属校验（Read 中带会话作用域的子集；
    /// Write 一律在 params 携带会话键时校验归属）。
    pub requires_session: bool,
    /// Process-wide diagnostics expose other sessions and require admin scope.
    pub admin_only: bool,
}

/// Wire params for the typed service-method slice. Optional TS derive is used
/// only by the contract exporter; serde stays the runtime authority.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionRenameParams {
    pub session_id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DaemonVersionParams {}

/// Remote filesystem browsing accepts an optional session scope that narrows
/// access to that session's workspace; it is not a lease-bound session call.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct FsListParams {
    pub path: String,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub scope_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct FsReadParams {
    pub path: String,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub max_bytes: Option<u64>,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub scope_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_file: bool,
    pub size: u64,
    pub modified_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct FsReadResult {
    pub path: String,
    pub size: u64,
    pub truncated: bool,
    pub content: String,
}

/// `session.list` 无参数；params 恒为 `{}`。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionListParams {}

/// `session.meta` 定位单条会话；meta 不存在时 result 为 `null`（键不缺、
/// 整个结果就是 null）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionMetaParams {
    pub session_id: String,
}

/// A typed descriptor links the method key, the exact wire DTOs and the
/// authorization metadata. Only methods present here are yet exposed as typed
/// ServiceMap calls; the remaining legacy RPCs stay explicitly untyped/pending.
#[derive(Debug, Clone, Copy)]
pub struct TypedMethodInfo {
    pub method: &'static str,
    pub params_type: &'static str,
    pub result_type: &'static str,
    pub info: MethodInfo,
    pub validate_params: fn(&Value) -> Result<(), String>,
    pub validate_result: fn(&Value) -> Result<(), String>,
}

fn validate_daemon_version_params(value: &Value) -> Result<(), String> {
    validate_params::<DaemonVersionParams>(value)
}

fn validate_string_result(value: &Value) -> Result<(), String> {
    validate_result::<String>(value)
}

fn validate_session_rename_params(value: &Value) -> Result<(), String> {
    validate_params::<SessionRenameParams>(value)
}

fn validate_null_result(value: &Value) -> Result<(), String> {
    validate_result::<Option<()>>(value)
}

fn validate_fs_list_params(value: &Value) -> Result<(), String> {
    validate_params::<FsListParams>(value)
}

fn validate_fs_read_params(value: &Value) -> Result<(), String> {
    validate_params::<FsReadParams>(value)
}

fn validate_fs_list_result(value: &Value) -> Result<(), String> {
    validate_result::<Vec<FsEntry>>(value)
}

fn validate_fs_read_result(value: &Value) -> Result<(), String> {
    validate_result::<FsReadResult>(value)
}

fn validate_session_list_params(value: &Value) -> Result<(), String> {
    validate_params::<SessionListParams>(value)
}

fn validate_session_meta_params(value: &Value) -> Result<(), String> {
    validate_params::<SessionMetaParams>(value)
}

fn validate_session_list_result(value: &Value) -> Result<(), String> {
    validate_result::<Vec<qaqh_types::SessionListEntry>>(value)
}

fn validate_session_meta_result(value: &Value) -> Result<(), String> {
    validate_result::<Option<qaqh_types::SessionListEntry>>(value)
}

pub const TYPED_METHODS: &[TypedMethodInfo] = &[
    TypedMethodInfo {
        method: "daemon.version",
        params_type: "DaemonVersionParams",
        result_type: "string",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_daemon_version_params,
        validate_result: validate_string_result,
    },
    TypedMethodInfo {
        method: "session.rename",
        params_type: "SessionRenameParams",
        result_type: "null",
        info: MethodInfo {
            kind: MethodKind::Write,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_session_rename_params,
        validate_result: validate_null_result,
    },
    TypedMethodInfo {
        method: "fs.list",
        params_type: "FsListParams",
        result_type: "FsEntry[]",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_fs_list_params,
        validate_result: validate_fs_list_result,
    },
    TypedMethodInfo {
        method: "fs.read",
        params_type: "FsReadParams",
        result_type: "FsReadResult",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_fs_read_params,
        validate_result: validate_fs_read_result,
    },
    TypedMethodInfo {
        method: "session.list",
        params_type: "SessionListParams",
        // 条目即前端契约 G2 的 SessionListEntry（SessionMeta flatten + 运行期字段）。
        result_type: "SessionListEntry[]",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_session_list_params,
        validate_result: validate_session_list_result,
    },
    TypedMethodInfo {
        method: "session.meta",
        params_type: "SessionMetaParams",
        // meta 缺失时整个 result 是 null（不是缺键）——`?` 后缀由 exporter
        // 映射为 `SessionListEntry | null`。
        result_type: "SessionListEntry?",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_session_meta_params,
        validate_result: validate_session_meta_result,
    },
];

fn validate_params<T: DeserializeOwned>(params: &Value) -> Result<(), String> {
    serde_json::from_value::<T>(params.clone())
        .map(|_| ())
        .map_err(|error| format!("invalid typed service params: {error}"))
}

fn validate_result<T: DeserializeOwned>(result: &Value) -> Result<(), String> {
    serde_json::from_value::<T>(result.clone())
        .map(|_| ())
        .map_err(|error| format!("typed service result does not match descriptor: {error}"))
}

/// Apply the DTO boundary for methods declared in [`TYPED_METHODS`].
/// Unknown methods return `Ok(None)` for the legacy dispatch path.
pub fn validate_typed_params(method: &str, params: &Value) -> Result<bool, String> {
    let Some(descriptor) = TYPED_METHODS
        .iter()
        .find(|descriptor| descriptor.method == method)
    else {
        return Ok(false);
    };
    (descriptor.validate_params)(params)?;
    Ok(true)
}

pub fn validate_typed_result(method: &str, result: &Value) -> Result<bool, String> {
    let Some(descriptor) = TYPED_METHODS
        .iter()
        .find(|descriptor| descriptor.method == method)
    else {
        return Ok(false);
    };
    (descriptor.validate_result)(result)?;
    Ok(true)
}

/// 服务面会话参数键。
pub const SESSION_PARAM: &str = "session_id";

/// 从服务面 params 取会话键。
pub fn session_param_value(params: &Value) -> Option<&str> {
    params.get(SESSION_PARAM).and_then(Value::as_str)
}

/// 取会话键并要求存在，缺失时报 `missing string parameter: session_id`。
pub fn session_param(params: &Value) -> Result<String, String> {
    session_param_value(params)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {SESSION_PARAM}"))
}

/// fs 远端作用域键。
pub fn scope_session_param_value(params: &Value) -> Option<&str> {
    params.get("scope_session_id").and_then(Value::as_str)
}

const READ: MethodInfo = MethodInfo {
    kind: MethodKind::Read,
    requires_session: false,
    admin_only: false,
};
const READ_ADMIN: MethodInfo = MethodInfo {
    kind: MethodKind::Read,
    requires_session: false,
    admin_only: true,
};
const READ_SEEDED: MethodInfo = MethodInfo {
    kind: MethodKind::Read,
    requires_session: true,
    admin_only: false,
};
const WRITE: MethodInfo = MethodInfo {
    kind: MethodKind::Write,
    requires_session: false,
    admin_only: false,
};
const WRITE_ADMIN: MethodInfo = MethodInfo {
    kind: MethodKind::Write,
    requires_session: false,
    admin_only: true,
};
const WRITE_SEEDED: MethodInfo = MethodInfo {
    kind: MethodKind::Write,
    requires_session: true,
    admin_only: false,
};

/// 方法表**本体**（单一真源）。
///
/// 之所以是「一个可遍历的数组」而不是 `match`：穷举闸必须能**从表反查清单**
/// （「表里留了一条没人实现的死条目 / 清单里漏了一条」两种漂移才都能被机械发现）。
/// 2026-10 之前这里是一个 `match`，两段所谓「反向」测试遍历的都是各 crate 的
/// `methods::ALL`——方向相同，等于空转；`dsaccount.verify` 就是那样漏掉的。
///
/// 顺序无语义（[`lookup`] 线性扫描，条目数几十，一次 RPC 一次查找）。
pub const METHODS: &[(&str, MethodInfo)] = &[
    // daemon / 会话只读
    ("daemon.version", READ),
    ("session.list", READ),
    ("session.meta", READ_SEEDED),
    ("session.activity", READ),
    ("session.dashboard", READ_SEEDED),
    ("session.get_activity", READ_SEEDED),
    // 会话级 profile 选择：写 meta.profile 并定向重载该会话（不动全局）。
    ("session.set_profile", WRITE_SEEDED),
    // 用户重命名会话标题（方案B）：写 meta.title 并广播 TitleChanged(user)。
    ("session.rename", WRITE_SEEDED),
    // workspace
    ("workspace.get", READ_SEEDED),
    ("workspace.list", READ),
    ("workspace.set", WRITE_SEEDED),
    ("workspace.create", WRITE),
    ("workspace.rename", WRITE),
    ("workspace.delete", WRITE),
    ("workspace.move_session", WRITE_SEEDED),
    ("workspace.detach", WRITE_SEEDED),
    // fs
    ("fs.list", READ),
    ("fs.read", READ),
    // config / profile
    ("config.load", READ),
    ("config.save", WRITE),
    ("config.set_permission_level", WRITE),
    ("profile.apply", WRITE),
    ("profile.save_current", WRITE),
    ("profile.delete", WRITE),
    // Sign in with ChatGPT（套餐用量）。**全部 admin_only**：配对出来的设备
    // （view/interact）绝不得触发登录 / 切账户 / 登出——那等于让远端设备把
    // 新账户接进用户机器并改写本机凭据。方法名的单一真源在 qaqh_siwc::methods。
    ("siwc.status", READ_ADMIN),
    ("siwc.profiles", READ_ADMIN),
    ("siwc.models", READ_ADMIN),
    ("siwc.sign_in.start", WRITE_ADMIN),
    ("siwc.sign_in.status", READ_ADMIN),
    ("siwc.sign_in.cancel", WRITE_ADMIN),
    ("siwc.profile.select", WRITE_ADMIN),
    ("siwc.sign_out", WRITE_ADMIN),
    // DeepSeek 账号（账号余额）。与 siwc 同一档待遇：**全部 admin_only**——
    // 配对设备不得触发登录 / 登出 / 读余额。方法名真源在 qaqh_dsaccount::methods。
    ("dsaccount.status", READ_ADMIN),
    ("dsaccount.balance", READ_ADMIN),
    ("dsaccount.bonuses", READ_ADMIN),
    ("dsaccount.sign_in.start", WRITE_ADMIN),
    ("dsaccount.sign_in.status", READ_ADMIN),
    ("dsaccount.sign_in.cancel", WRITE_ADMIN),
    ("dsaccount.sign_out", WRITE_ADMIN),
    ("dsaccount.bonus.ack", WRITE_ADMIN),
    ("dsaccount.models", READ_ADMIN),
    // 可能顺手清凭据，所以按写类报错（`action_failed`）而不是只读。
    ("dsaccount.verify", WRITE_ADMIN),
    // skills
    ("skills.list_tools", READ),
    ("skills.operation", WRITE),
    ("skills.reload", WRITE),
    // todo / plan / stats
    ("todo.status", READ_SEEDED),
    // todo CLI 路线（daemon HTTP 直访；与 LLM 工具分发表共用 exec 核心）
    ("todo.list", READ_SEEDED),
    ("todo.set", WRITE_SEEDED),
    ("plan.read", READ_SEEDED),
    // Process-wide memory view is intentionally admin-scope only.
    ("diagnostics.memory.snapshot", READ_ADMIN),
    ("diagnostics.memory.start", WRITE_ADMIN),
    ("diagnostics.memory.stop", WRITE_ADMIN),
    // git（只读与变更分列）
    ("git.diff", READ_SEEDED),
    ("git.branch", READ_SEEDED),
    ("git.branches", READ_SEEDED),
    ("git.file_diff", READ_SEEDED),
    ("git.switch_branch", WRITE),
    ("git.commit", WRITE),
    // subagent / tool mode
    ("subagent.spawn", WRITE),
    ("session.set_tool_mode", WRITE_SEEDED),
];

/// 方法表：未列出的名字返回 `None`（HTTP 404）。
pub fn lookup(method: &str) -> Option<MethodInfo> {
    METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, info)| *info)
}

/// 统一错误响应形状（daemon HTTP 层使用），按方法类别区分 code。
pub fn error_response(kind: MethodKind, message: &str) -> Value {
    // 白名单/边界拒绝（`fs.read`/`fs.list` 的 `FORBIDDEN:` 前缀）单独成码：
    // 调用方必须能把「出白名单」与「查询本身失败（IO / 未知）」区分开
    // （T-2-1 验收：拒绝返回 FORBIDDEN，而非伪装成 IO_ERROR）。
    let code = if message.starts_with("FORBIDDEN") {
        "forbidden"
    } else {
        match kind {
            MethodKind::Read => "query_failed",
            MethodKind::Write => "action_failed",
        }
    };
    serde_json::json!({ "code": code, "message": message })
}

/// 服务分发：方法表校验后的唯一入口。
///
/// 账号面（siwc/dsaccount）先走 [`accounts::dispatch`] 的逐方法 DTO 收口
/// （params 解码 → 调用管理器 → DTO 投影），其余方法经 `service.handle`
/// 执行并按 [`TYPED_METHODS`] 做边界校验。
pub fn dispatch(service: &QaqhService, method: &str, params: &Value) -> Result<Value, String> {
    if let Some(result) = accounts::dispatch(service, method, params) {
        return result;
    }
    let typed = validate_typed_params(method, params)?;
    let result = service.handle(method, params)?;
    if typed {
        validate_typed_result(method, &result)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「登录/凭据」类方法一律 admin-only，且方法表与 qaqh-siwc 的清单**逐一对齐**。
    ///
    /// 这条测试的意义不在覆盖率，而在**越权面**：任何新增的 `siwc.*` 方法
    /// 只要漏了 admin 档，就会被这里挡住（`ALL` 是那份清单的真源）。
    ///
    /// **两向都遍历**：`ALL → 表`（漏项）与 `表 → ALL`（死条目）。第二向必须从
    /// [`METHODS`] 出发——遍历 `ALL` 再查表与正向是同一个方向，等于空转。
    #[test]
    fn siwc_methods_are_admin_only_and_fully_enumerated() {
        for method in qaqh_siwc::methods::ALL {
            let info = lookup(method).unwrap_or_else(|| panic!("方法表缺少 {method}"));
            assert!(
                info.admin_only,
                "{method} 必须 admin_only：配对设备不得改写本机凭据"
            );
        }
        // 反向（真反向）：表里的 siwc.* 一个都不能多。
        for (method, info) in METHODS {
            if method.starts_with("siwc.") {
                assert!(
                    qaqh_siwc::methods::ALL.contains(method),
                    "{method} 在方法表里，却不在 qaqh_siwc::methods::ALL"
                );
                assert!(info.admin_only, "{method} 必须 admin_only");
            }
        }
    }

    /// DeepSeek 账号：与 siwc 同一档（全部 admin_only + 清单全覆盖）。
    ///
    /// 越权面的意义与上面那条相同：账号凭据同样是「本机身份」，配对设备
    /// 不得碰它——尤其 `sign_in.start`（会打开浏览器并把新账号接进本机）。
    ///
    /// 这条的**反向**臂是 R5 的验收项：当初 `dsaccount.verify` 在表里、
    /// 不在 `ALL` 里，而旧的「反向」遍历的仍是 `ALL`，所以从来没红过。
    #[test]
    fn dsaccount_methods_are_admin_only_and_fully_enumerated() {
        assert!(!qaqh_dsaccount::methods::ALL.is_empty());
        for method in qaqh_dsaccount::methods::ALL {
            let info = lookup(method).unwrap_or_else(|| panic!("方法表缺少 {method}"));
            assert!(
                info.admin_only,
                "{method} 必须 admin_only：配对设备不得触碰本机账号凭据"
            );
        }
        // 反向（真反向）：表里的 dsaccount.* 一个都不能多。
        for (method, info) in METHODS {
            if method.starts_with("dsaccount.") {
                assert!(
                    qaqh_dsaccount::methods::ALL.contains(method),
                    "{method} 在方法表里，却不在 qaqh_dsaccount::methods::ALL"
                );
                assert!(info.admin_only, "{method} 必须 admin_only");
            }
        }
    }

    /// 方法表可遍历之后，重名条目也该被挡住：`lookup` 只命中第一条，
    /// 多出来的那条等于「写了但永远不生效」——旧 `match` 形态会直接编译失败，
    /// 换成数组之后必须由测试补上这条闸。
    #[test]
    fn method_table_has_no_duplicate_names() {
        let mut names: Vec<&str> = METHODS.iter().map(|(name, _)| *name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "方法表里有重名条目");
    }

    #[test]
    fn typed_descriptors_match_runtime_method_metadata() {
        // 两份 descriptor 清单（本文件与账号面）合起来看：逐条对上方法表，
        // 且跨清单不得重复声明同一方法。
        let combined: Vec<&TypedMethodInfo> = TYPED_METHODS
            .iter()
            .chain(accounts::TYPED_METHODS.iter())
            .collect();
        for descriptor in &combined {
            let runtime = lookup(descriptor.method).unwrap_or_else(|| {
                panic!(
                    "typed descriptor has no runtime method: {}",
                    descriptor.method
                )
            });
            assert_eq!(
                runtime, descriptor.info,
                "metadata mismatch for {}",
                descriptor.method
            );
        }
        let mut methods: Vec<_> = combined
            .iter()
            .map(|descriptor| descriptor.method)
            .collect();
        let count = methods.len();
        methods.sort_unstable();
        methods.dedup();
        assert_eq!(methods.len(), count, "typed descriptor method duplicated");
    }

    #[test]
    fn typed_method_params_fail_before_runtime_dispatch() {
        assert!(
            validate_typed_params(
                "session.rename",
                &serde_json::json!({"session_id": "s1", "title": 4})
            )
            .is_err()
        );
        assert_eq!(
            validate_typed_params(
                "session.rename",
                &serde_json::json!({"session_id": "s1", "title": "new title"})
            )
            .expect("valid params"),
            true
        );
        assert_eq!(
            validate_typed_params("session.list", &serde_json::json!({})).expect("typed"),
            true
        );
        assert!(
            validate_typed_params("session.meta", &serde_json::json!({"session_id": "s1"}))
                .expect("typed")
        );
        assert!(
            validate_typed_params("session.meta", &serde_json::json!({})).is_err(),
            "session.meta 必须带 session_id"
        );
        assert!(
            validate_typed_params(
                "fs.list",
                &serde_json::json!({ "path": 3, "scope_session_id": "s1" })
            )
            .is_err()
        );
        assert!(
            validate_typed_params(
                "fs.read",
                &serde_json::json!({ "path": "/tmp/a", "max_bytes": "large" })
            )
            .is_err()
        );
        assert!(
            validate_typed_params(
                "fs.read",
                &serde_json::json!({ "path": "/tmp/a", "scope_session_id": "s1" })
            )
            .expect("optional max_bytes and scope are accepted")
        );
    }

    #[test]
    fn typed_method_results_match_descriptors() {
        assert!(
            validate_typed_result("daemon.version", &serde_json::json!("2.0.0"))
                .expect("valid result")
        );
        assert!(validate_typed_result("daemon.version", &serde_json::json!({})).is_err());
        assert!(validate_typed_result("session.rename", &Value::Null).expect("null result"));
        assert!(
            validate_typed_result("session.list", &serde_json::json!([])).expect("typed result")
        );
        assert!(
            validate_typed_result("session.list", &serde_json::json!([{}])).is_err(),
            "空对象不是 SessionListEntry（G2 形状必须响亮失败）"
        );
        assert!(
            validate_typed_result("session.meta", &Value::Null)
                .expect("meta 缺失 → null 是合法结果")
        );
        assert!(
            validate_typed_result("session.meta", &serde_json::json!([])).is_err(),
            "数组不是 session.meta 的结果形状"
        );
        assert!(validate_typed_result(
            "fs.list",
            &serde_json::json!([{ "name": "a", "path": "/a", "is_dir": false, "is_file": true, "size": 1, "modified_ms": null }])
        )
        .expect("valid listing"));
        assert!(validate_typed_result("fs.list", &serde_json::json!([{ "name": "a" }])).is_err());
        assert!(
            validate_typed_result(
                "fs.read",
                &serde_json::json!({ "path": "/a", "size": 1, "truncated": false, "content": "a" })
            )
            .expect("valid read")
        );
        assert!(validate_typed_result(
            "fs.read",
            &serde_json::json!({ "path": "/a", "size": "1", "truncated": false, "content": "a" })
        )
        .is_err());
    }

    /// 客户端服务面白名单（R1）必须与方法表**两向**对齐：
    /// - 两份 `methods::ALL` ⊆ `qaqh_client::SERVICE_METHODS`（不漏）；
    /// - 白名单里没有表外的死条目（不虚）。
    ///
    /// 这条闸放在 runtime 而不是 client：只有这里同时看得见方法表、
    /// 两家 `methods` 与客户端白名单。
    #[test]
    fn client_service_whitelist_mirrors_account_methods() {
        let declared: Vec<&str> = qaqh_siwc::methods::ALL
            .iter()
            .chain(qaqh_dsaccount::methods::ALL.iter())
            .copied()
            .collect();
        for method in &declared {
            assert!(
                qaqh_client::service::is_allowed(method),
                "{method} 在 methods::ALL 里，但客户端白名单没有放行"
            );
            assert!(
                lookup(method).is_some(),
                "{method} 在 methods::ALL 里，但 daemon 方法表没有它"
            );
        }
        // 反向：白名单里的每条都得是表里真实存在的 admin 方法。
        for method in qaqh_client::service::SERVICE_METHODS {
            let info = lookup(method)
                .unwrap_or_else(|| panic!("客户端白名单里的 {method} 不在 daemon 方法表"));
            assert!(info.admin_only, "{method} 应是 admin_only（账号凭据面）");
            assert!(
                declared.contains(method),
                "客户端白名单里的 {method} 不在任何 methods::ALL 里（死条目）"
            );
        }
    }

    /// 服务面会话键只认 `session_id`：legacy `seed` 已随 Phase E 退场。
    #[test]
    fn session_param_only_accepts_session_id() {
        let modern = serde_json::json!({ "session_id": "new" });
        assert_eq!(session_param_value(&modern), Some("new"));
        assert_eq!(session_param(&modern).unwrap(), "new");

        // 旧键不再被接受。
        let legacy = serde_json::json!({ "seed": "old" });
        assert_eq!(session_param_value(&legacy), None);
        assert!(session_param(&legacy).is_err());

        // 两个都在时也只取 `session_id`。
        let both = serde_json::json!({ "session_id": "new", "seed": "old" });
        assert_eq!(session_param_value(&both), Some("new"));

        assert_eq!(session_param_value(&serde_json::json!({})), None);
        assert!(session_param(&serde_json::json!({})).is_err());
    }

    /// fs 作用域键同款：只认 `scope_session_id`。
    #[test]
    fn scope_session_param_only_accepts_scope_session_id() {
        assert_eq!(
            scope_session_param_value(&serde_json::json!({ "scope_session_id": "new" })),
            Some("new")
        );
        assert_eq!(
            scope_session_param_value(&serde_json::json!({ "scope_seed": "old" })),
            None
        );
        assert_eq!(
            scope_session_param_value(&serde_json::json!({
                "scope_session_id": "new",
                "scope_seed": "old"
            })),
            Some("new")
        );
        assert_eq!(scope_session_param_value(&serde_json::json!({})), None);
    }

    // SessionManager 是全局单例，同一测试进程只能 init 一次；
    // 用 OnceLock 共享一个 service 实例（并行测试也不会重复初始化）。
    static SERVICE: std::sync::OnceLock<QaqhService> = std::sync::OnceLock::new();

    fn service() -> &'static QaqhService {
        SERVICE.get_or_init(|| {
            // 隔离 data root：lib 单测不读/不写真实用户根（sessions/、
            // workspaces.json），与其他测试二进制同一纪律。
            static ISOLATED_DATA_ROOT: std::sync::OnceLock<tempfile::TempDir> =
                std::sync::OnceLock::new();
            let tmp = ISOLATED_DATA_ROOT.get_or_init(|| {
                let tmp = tempfile::tempdir().expect("isolated data root tempdir");
                // SAFETY: OnceLock 初始化窗口内的单次进程级设置，此后只读。
                unsafe {
                    std::env::set_var("QAQH_DATA_DIR", tmp.path().join(".qaqh"));
                    std::env::set_var("HOME", tmp.path());
                    std::env::set_var("USERPROFILE", tmp.path());
                }
                tmp
            });
            let _ = tmp;
            qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
            QaqhService::init(qaqh_session::SessionManager::global())
        })
    }

    #[test]
    fn todo_cli_methods_are_registered() {
        let set = lookup("todo.set").expect("todo.set registered");
        assert_eq!(set.kind, MethodKind::Write);
        assert!(set.requires_session, "todo.set 必须携带 seed 并做归属校验");
        let list = lookup("todo.list").expect("todo.list registered");
        assert_eq!(list.kind, MethodKind::Read);
        assert!(list.requires_session);
        assert!(lookup("todo.set ").is_none(), "方法名不容尾随空格");

        for method in [
            "workspace.set",
            "workspace.move_session",
            "workspace.detach",
            "session.set_tool_mode",
        ] {
            let info = lookup(method).unwrap_or_else(|| panic!("{method} registered"));
            assert_eq!(info.kind, MethodKind::Write, "{method}");
            assert!(
                info.requires_session,
                "{method} must require seed ownership"
            );
        }
    }

    #[test]
    fn session_list_returns_array() {
        let result = dispatch(service(), "session.list", &serde_json::json!({})).expect("list");
        assert!(result.is_array());
    }

    /// **G2 回归闸（产出边界）**：`session.list` 的条目必须能被权威类型吃下，
    /// 且类型化后再序列化与原 wire **逐键相同**（不多键、不少键）。
    ///
    /// 注意覆盖面：本机数据目录为空时这个循环退化为空——条目形状本身由
    /// `qaqh-types::session::tests::session_list_entry_wire_keys_are_locked`
    /// 锁住（那份不依赖任何数据）。这里补的是「产出方真的发的是那个形状」。
    #[test]
    fn session_list_entries_are_typed_at_the_boundary() {
        let wire = dispatch(service(), "session.list", &serde_json::json!({})).expect("list");
        let entries: Vec<qaqh_types::SessionListEntry> =
            serde_json::from_value(wire.clone()).expect("G2：条目必须能被权威类型解析");
        assert_eq!(entries.len(), wire.as_array().unwrap().len());
        for (typed, raw) in entries.iter().zip(wire.as_array().unwrap()) {
            assert_eq!(
                &serde_json::to_value(typed).expect("serialize"),
                raw,
                "类型化往返改了 wire 形状"
            );
        }
    }

    #[test]
    fn read_methods_carry_read_kind() {
        let info = lookup("session.list").expect("listed");
        assert_eq!(info.kind, MethodKind::Read);
        assert!(!info.requires_session);
        let info = lookup("session.meta").expect("listed");
        assert!(info.requires_session);
    }

    #[test]
    fn lifecycle_methods_are_deliberately_absent() {
        // 会话生命周期只走 command envelope（N5），服务面不收。
        assert!(lookup("session.new").is_none());
        assert!(lookup("session.resume").is_none());
        assert!(lookup("skills.activate").is_none());
        assert!(lookup("todo.cancel").is_none());
        assert!(lookup("plan.action").is_none());
    }

    #[test]
    fn slash_alias_is_no_longer_accepted() {
        // 旧双别名（"session/list"）已拆除：单一规范形态 `module.method`。
        assert!(lookup("session/list").is_none());
    }

    #[test]
    fn error_response_shape_differs_by_kind() {
        assert_eq!(
            error_response(MethodKind::Read, "x")["code"],
            serde_json::json!("query_failed")
        );
        assert_eq!(
            error_response(MethodKind::Write, "x")["code"],
            serde_json::json!("action_failed")
        );
    }

    /// T-2-1：白名单拒绝必须成 `forbidden` 码，而不是伪装成 `query_failed`
    /// （与 IO 失败不可区分）。
    #[test]
    fn forbidden_prefix_maps_to_forbidden_code() {
        for kind in [MethodKind::Read, MethodKind::Write] {
            let value = error_response(
                kind,
                "FORBIDDEN: fs.read /etc/passwd: path is outside the allowed roots",
            );
            assert_eq!(value["code"], serde_json::json!("forbidden"));
            assert!(
                value["message"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("FORBIDDEN")
            );
        }
    }
}
