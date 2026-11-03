//! `todo.*` / `plan.*` 服务面。
//!
//! `todo.set` / `todo.list` 的业务字段表在 `qaqh_workspace::todo` 的 typed
//! 入口里，这里只拆会话键、透传其余字段——不抄第三份字段表。

use serde_json::Value;

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;
use crate::service::command::TaskCommand;
use crate::service::plan::{plan_action, read_plan};

pub(crate) fn handle(service: &QaqhService, command: TaskCommand) -> QaqhResult<Value> {
    match command {
        TaskCommand::TodoStatus(args) => {
            qaqh_workspace::todo::todo_status_value(&args.session_id).map_err(QaqhError::message)
        }
        // PublishResourceChanged 已退出 UI 命令目录(报告三 §6.3):todo 变化
        // 由客户端在下一次读取时可见,不再有 fact 通知。
        TaskCommand::TodoCancel(args) => {
            qaqh_workspace::todo::todo_cancel_value(&args.session_id, &args.id)
                .map_err(QaqhError::message)
        }
        TaskCommand::TodoSet(args) => {
            qaqh_workspace::todo::todo_set_value_for(&args.session_id, &args.tool_args())
                .map_err(QaqhError::message)
        }
        TaskCommand::TodoList(args) => {
            qaqh_workspace::todo::todo_list_value_for(&args.session_id, &args.tool_args())
                .map_err(QaqhError::message)
        }
        TaskCommand::PlanRead(args) => {
            let plan = read_plan(&service.sessions, &args.session_id);
            QaqhError::to_value("plan", plan)
        }
        TaskCommand::PlanAction(args) => {
            let output = plan_action(
                &service.sessions,
                &args.session_id,
                &args.item_id,
                &args.action,
                args.user_comment.as_deref().unwrap_or_default(),
            )
            .map_err(QaqhError::message)?;
            QaqhError::to_value("plan action", output)
        }
    }
}
