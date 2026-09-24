//! pending interaction 正文的**单一序列化来源**（#345）。
//!
//! ask / plan 的 modal 正文属于展示面：canonical fact 只存 `ContentRef`，正文进
//! content store。为了让「引擎算出来的 ref」与「hub 写进 store 的 bytes」必然对
//! 得上，**两处都必须调用本模块的构造函数**——不要再各自 `serde_json::json!` 一份，
//! 否则 ref 会解析不到（哈希不一致），而且这种漂移不会有编译期错误。
//!
//! 约定：
//! - body 顶层带 `kind`，客户端按 `kind` 分派渲染；
//! - `ask` 的 `mode` / `questions` 与 v1 域事件同源，客户端**不需要**再做单/批归一化；
//! - `plan` 用 `review_type` 区分 `plan_submit` 与 `todo_activation`。

use crate::{AskMode, AskQuestion, PermissionCategory, PermissionRisk, PlanReviewItem};

/// 交互正文的 media type（content store / v2 content 端点共用）。
pub const INTERACTION_BODY_MEDIA_TYPE: &str = "application/vnd.qaqh.interaction-request+json";

/// ask 交互正文（`kind = "ask"`）。
pub fn ask_body(mode: AskMode, questions: &[AskQuestion]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "kind": "ask",
        "mode": mode,
        "questions": questions,
    }))
    .unwrap_or_default()
}

/// plan review 交互正文（`kind = "plan"`）。
///
/// `review_type` 取值与 v1 域事件一致：`plan`（plan_submit）/ `todo_activation`。
/// todo 激活的 `plan_content` 为空串（与 `admit.rs` 的域事件构造保持一致）。
pub fn plan_body(
    plan_content: &str,
    review_type: &str,
    todo_items: Option<&[PlanReviewItem]>,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "kind": "plan",
        "plan_content": plan_content,
        "review_type": review_type,
        "todo_items": todo_items,
    }))
    .unwrap_or_default()
}

/// permission 交互正文（`kind = "permission"`）。
///
/// #345 时 permission 刻意**没有**正文（详情只在 tool 频道快照 / timeline 卡里）。
/// 纯 v2 之后 wire 上不再有 tool 频道快照，壳层（TUI / webui gateway）只能从
/// canonical ref 取正文才能渲染授权面板 ⇒ 详情也必须走同一条路。
/// canonical ref 依赖逐字节一致，故参数与 wire 字段一一对应、不做结构体包装。
#[allow(clippy::too_many_arguments)]
pub fn permission_body(
    tool_name: &str,
    action_summary: Option<&str>,
    reason: &str,
    paths: &[String],
    category: PermissionCategory,
    level: u8,
    risk: PermissionRisk,
    consequence: &str,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "kind": "permission",
        "tool_name": tool_name,
        "action_summary": action_summary,
        "reason": reason,
        "paths": paths,
        "category": category,
        "level": level,
        "risk": risk,
        "consequence": consequence,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_body_carries_the_approval_details() {
        let paths = vec!["/tmp/x".to_string()];
        let body = permission_body(
            "exec",
            Some("run ls"),
            "needs shell",
            &paths,
            PermissionCategory::Exec,
            3,
            PermissionRisk::High,
            "runs a command",
        );
        let value: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
        assert_eq!(value["kind"], "permission");
        assert_eq!(value["tool_name"], "exec");
        assert_eq!(value["action_summary"], "run ls");
        assert_eq!(value["category"], "exec");
        assert_eq!(value["risk"], "high");
        assert_eq!(value["level"], 3);
        assert_eq!(value["paths"][0], "/tmp/x");
        // canonical ref 依赖逐字节一致。
        assert_eq!(
            body,
            permission_body(
                "exec",
                Some("run ls"),
                "needs shell",
                &paths,
                PermissionCategory::Exec,
                3,
                PermissionRisk::High,
                "runs a command",
            )
        );
    }
    fn question(id: &str) -> AskQuestion {
        AskQuestion {
            id: id.to_string(),
            question: format!("Q {id}?"),
            options: vec!["a".into(), "b".into()],
            allow_custom: true,
        }
    }

    #[test]
    fn bodies_are_deterministic_for_identical_inputs() {
        let questions = [question("q1"), question("q2")];
        let first = ask_body(AskMode::Batch, &questions);
        let second = ask_body(AskMode::Batch, &questions);
        assert_eq!(first, second, "same input must serialize byte-identically");
        let value: serde_json::Value = serde_json::from_slice(&first).expect("valid json");
        assert_eq!(value["kind"], "ask");
        assert_eq!(value["mode"], "batch");
        assert_eq!(value["questions"].as_array().expect("array").len(), 2);
    }

    #[test]
    fn plan_body_carries_review_type_and_todo_items() {
        let plan: serde_json::Value =
            serde_json::from_slice(&plan_body("do it", "plan", None)).expect("valid json");
        assert_eq!(plan["kind"], "plan");
        assert_eq!(plan["plan_content"], "do it");
        assert_eq!(plan["review_type"], "plan");
        assert!(plan["todo_items"].is_null());

        let items = [PlanReviewItem {
            id: "t1".into(),
            title: "step".into(),
            description: "do the thing".into(),
            complexity: "simple".into(),
        }];
        let todo: serde_json::Value =
            serde_json::from_slice(&plan_body("", "todo_activation", Some(&items)))
                .expect("valid json");
        assert_eq!(todo["review_type"], "todo_activation");
        assert_eq!(todo["todo_items"].as_array().expect("array").len(), 1);
    }
}
