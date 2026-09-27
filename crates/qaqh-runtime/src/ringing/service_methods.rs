//! Ringing 服务面方法表（`POST /ringing/v2/service/{method}` 的单一权威清单）。
//!
//! 旧 `/queries/{name}`（闭表白名单）与 `/actions/{name}`（前缀 allowlist）
//! 双端点及其 slash/dot 双别名容忍已合并于此：一个方法一个条目，
//! `Read` = 无副作用查询，`Write` = 变更操作。
//!
//! 会话生命周期（`session.new`/`session.resume`/`skills.activate`/`todo.cancel`/
//! `plan.action` 等命令语义方法）刻意不在表中——会话命令只走三频道
//! command envelope（开发标准 N5），服务面仅承载非会话 RPC。

use serde_json::Value;

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
};
const READ_SEEDED: MethodInfo = MethodInfo {
    kind: MethodKind::Read,
    requires_session: true,
};
const WRITE: MethodInfo = MethodInfo {
    kind: MethodKind::Write,
    requires_session: false,
};
const WRITE_SEEDED: MethodInfo = MethodInfo {
    kind: MethodKind::Write,
    requires_session: true,
};

/// 方法表：未列出的名字返回 `None`（HTTP 404）。
pub fn lookup(method: &str) -> Option<MethodInfo> {
    match method {
        // daemon / 会话只读
        "daemon.version" => Some(READ),
        "session.list" => Some(READ),
        "session.meta" => Some(READ_SEEDED),
        "session.activity" => Some(READ),
        "session.dashboard" => Some(READ_SEEDED),
        "session.get_activity" => Some(READ_SEEDED),
        // workspace
        "workspace.get" => Some(READ_SEEDED),
        "workspace.list" => Some(READ),
        "workspace.set" => Some(WRITE_SEEDED),
        "workspace.create" => Some(WRITE),
        "workspace.rename" => Some(WRITE),
        "workspace.delete" => Some(WRITE),
        "workspace.move_session" => Some(WRITE_SEEDED),
        "workspace.detach" => Some(WRITE_SEEDED),
        // fs
        "fs.list" => Some(READ),
        "fs.read" => Some(READ),
        // config / profile
        "config.load" => Some(READ),
        "config.save" => Some(WRITE),
        "config.set_permission_level" => Some(WRITE),
        "profile.apply" => Some(WRITE),
        "profile.save_current" => Some(WRITE),
        "profile.delete" => Some(WRITE),
        // skills
        "skills.list_tools" => Some(READ),
        "skills.operation" => Some(WRITE),
        "skills.reload" => Some(WRITE),
        // todo / plan / stats
        "todo.status" => Some(READ_SEEDED),
        // todo CLI 路线（daemon HTTP 直访；与 LLM 工具分发表共用 exec 核心）
        "todo.list" => Some(READ_SEEDED),
        "todo.set" => Some(WRITE_SEEDED),
        "plan.read" => Some(READ_SEEDED),
        "plan.context_stats" => Some(READ_SEEDED),
        "stats.token_usage" => Some(READ),
        // git（只读与变更分列）
        "git.diff" => Some(READ_SEEDED),
        "git.branch" => Some(READ_SEEDED),
        "git.branches" => Some(READ_SEEDED),
        "git.file_diff" => Some(READ_SEEDED),
        "git.switch_branch" => Some(WRITE),
        "git.commit" => Some(WRITE),
        // subagent / tool mode
        "subagent.spawn" => Some(WRITE),
        "session.set_tool_mode" => Some(WRITE_SEEDED),
        _ => None,
    }
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
pub fn dispatch(service: &QaqhService, method: &str, params: &Value) -> Result<Value, String> {
    service.handle(method, params)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn service() -> &'static QaqhService {
        SERVICE.get_or_init(|| {
            qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
            QaqhService::init(qaqh_session::SessionManager::global())
        })
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
