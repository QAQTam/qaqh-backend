//! `todo.*` / `plan.*` 服务面。
//!
//! `todo.set` / `todo.list` 的业务字段表在 `qaqh_workspace::todo` 的 typed
//! 入口里，这里只拆会话键、透传其余字段——不抄第三份字段表。

use serde_json::Value;

use crate::error::{QaqhError, QaqhResult};
use qaqh_ringing::{CommandTarget, RingingCommand};

use crate::service::QaqhService;
use crate::service::command::TaskCommand;
use crate::service::plan::{plan_action, read_plan};

pub(crate) fn handle(service: &QaqhService, command: TaskCommand) -> QaqhResult<Value> {
    match command {
        TaskCommand::TodoStatus(args) => {
            qaqh_workspace::todo::todo_status_value(&args.session_id).map_err(QaqhError::message)
        }
        TaskCommand::TodoList(args) => {
            // B08:service 查询走纯快照 DTO(无工具信封键),status 词表与
            // todo 工具一致。
            qaqh_workspace::todo::todo_list_snapshot_value(&args.session_id, args.status.as_deref())
                .map_err(QaqhError::message)
        }
        TaskCommand::PlanRead(args) => {
            // 先解 Result 再序列化:serde 会把 `Result<T, E>` 外部标记成
            // `{"Ok": …}`/`{"Err": …}` map,直接 to_value 会撞上
            // `PlanReadItem[]` 描述符(真机验收 2026-11-04 实证)。
            let plan =
                read_plan(&service.owners, &args.session_id).map_err(QaqhError::message)?;
            QaqhError::to_value("plan", plan)
        }
    }
}

pub(crate) fn handle_ringing(
    service: &QaqhService,
    target: &CommandTarget,
    command: &RingingCommand,
) -> QaqhResult<Value> {
    let CommandTarget::Session { session_id } = target else {
        return Err(QaqhError::invalid(
            "task mutation requires a session target",
        ));
    };
    match command {
        RingingCommand::TodoSet { args } => {
            qaqh_workspace::todo::todo_set_value_for(session_id, args).map_err(QaqhError::message)
        }
        RingingCommand::TodoCancel { id } => {
            qaqh_workspace::todo::todo_cancel_value(session_id, id).map_err(QaqhError::message)
        }
        RingingCommand::PlanAction {
            item_id,
            action,
            user_comment,
        } => {
            let output = plan_action(
                &service.owners,
                session_id,
                item_id,
                action,
                user_comment.as_deref().unwrap_or_default(),
            )
            .map_err(QaqhError::message)?;
            QaqhError::to_value("plan action", output)
        }
        _ => Err(QaqhError::invalid(format!(
            "{} is not a task mutation",
            command.name()
        ))),
    }
}
