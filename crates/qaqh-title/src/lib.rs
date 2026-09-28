//! Session title generation.
//!
//! Split out of the agent loop: the loop only decides *when* to title and
//! applies the stateful fallback; this crate owns the pure text logic and the
//! background LLM summary task.
//!
//! 时序设计（保证"立刻可见" + "质量优先"）：
//! 1. **主线程**（调用方）：turn 终态挂点用首条用户消息截断生成标题，立刻写盘广播；
//! 2. **后台线程**（[`spawn_summary`]）：`chat_sync` 一次小调用（≤64 tokens）
//!    做 LLM 总结，成功后**覆盖**截断版；失败/超时保持截断版（降级路径）。
//! 3. **冻结**：由调用方守卫（`title.is_some()` 后不再生成）。

use std::sync::Arc;

use qaqh_session::SessionManager;

/// LLM 标题生成 prompt（专用小调用）。
pub const TITLE_SYSTEM: &str = "你是会话标题生成器。根据用户的第一条消息，用不超过 20 个字符的中文概括其需求。只输出标题本身：不要引号、不要标点、不要解释、不要换行。";

/// 截断标题上限（字符）。
pub const FALLBACK_MAX_CHARS: usize = 20;
/// LLM 标题清洗上限（字符）。
pub const LLM_MAX_CHARS: usize = 30;

/// 首条 user 消息的纯文本（取第一个 text block；无则 None）。
pub fn first_user_text(store: &qaqh_message::MessageStore) -> Option<String> {
    store.to_vec().into_iter().find_map(|m| {
        if m.role != "user" {
            return None;
        }
        m.content.iter().find_map(|b| match b {
            qaqh_types::ContentBlock::Text { text } if !text.trim().is_empty() => {
                Some(text.clone())
            }
            _ => None,
        })
    })
}

/// 截断降级：去 markdown 装饰 + 折叠空白 + 取前 N 字符。
pub fn truncate_title(raw: &str) -> String {
    let mut cleaned: String = raw
        .chars()
        .filter(|c| !matches!(c, '#' | '*' | '`' | '>' | '-' | '_' | '~'))
        .collect();
    // 折叠连续空白为单空格（含换行）。
    let mut out = String::with_capacity(cleaned.len());
    let mut prev_space = false;
    for c in cleaned.drain(..) {
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let out = out.trim();
    out.chars().take(FALLBACK_MAX_CHARS).collect()
}

/// LLM 输出清洗：剥引号/首尾空白/换行 → 截断。
pub fn clean_title(raw: &str) -> String {
    let mut out = raw.trim().to_string();
    // 剥包裹引号（中文/英文成对）。
    let chars: Vec<char> = out.chars().collect();
    if chars.len() >= 2 {
        let (head, tail) = (chars[0], chars[chars.len() - 1]);
        if matches!(
            (head, tail),
            ('"', '"') | ('\'', '\'') | ('“', '”') | ('「', '」')
        ) {
            out = chars[1..chars.len() - 1].iter().collect();
        }
    }
    // 折叠空白（LLM 可能输出换行/多空格）。
    let mut folded = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !prev_space && !folded.is_empty() {
                folded.push(' ');
            }
            prev_space = true;
        } else {
            folded.push(c);
            prev_space = false;
        }
    }
    let folded = folded.trim();
    folded.chars().take(LLM_MAX_CHARS).collect()
}

/// One background LLM summary task.
pub struct SummaryTask {
    /// Provider config built by the caller (gate stays the only provider face).
    pub provider: qaqh_gate::ProviderConfig,
    pub session_id: String,
    /// 首条用户消息（标题的语义锚点）。
    pub user_msg: String,
    /// 注入句柄：后台线程直接写盘（不参与调用方写序）。
    pub session_manager: Option<Arc<SessionManager>>,
    /// LLM 总结成功后的回调（收到的 title 已清洗）；调用方用它广播
    /// `SessionMetaChanged`。失败/空标题时不触发。
    pub on_title: Option<Box<dyn FnOnce(String) + Send>>,
}

/// Spawn the background summary thread (best-effort; failure only logs).
pub fn spawn_summary(task: SummaryTask) {
    let SummaryTask {
        provider,
        session_id,
        user_msg,
        session_manager,
        on_title,
    } = task;
    let spawned = std::thread::Builder::new()
        .name("session-title".into())
        .spawn(move || {
            let text = match qaqh_gate::chat_sync(
                &provider,
                vec![
                    qaqh_types::Message::system(TITLE_SYSTEM),
                    qaqh_types::Message::user(&user_msg),
                ],
                64,
            ) {
                Ok(text) => text,
                Err(error) => {
                    log::warn!("[TITLE] LLM summary failed, keeping fallback: {error}");
                    return;
                }
            };
            let title = clean_title(&text);
            if title.is_empty() {
                return;
            }
            // 覆盖截断版（同一次生成流程，未冻结）。
            if let Some(sm) = session_manager {
                sm.update_title(&session_id, &title);
            }
            if let Some(on_title) = on_title {
                on_title(title);
            }
        });
    if let Err(error) = spawned {
        log::warn!("[TITLE] spawn summary thread failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_strips_markdown_and_folds_whitespace() {
        assert_eq!(
            truncate_title("## 帮我修复权限掉L1的问题"),
            "帮我修复权限掉L1的问题"
        );
        assert_eq!(
            truncate_title("- 运行 cargo test\n- 看看结果"),
            "运行 cargo test 看看结果"
        );
        assert_eq!(
            truncate_title("这是一个超过二十个字符的非常长的用户需求描述文本内容"),
            "这是一个超过二十个字符的非常长的用户需求"
        );
    }

    #[test]
    fn clean_strips_quotes_and_truncates() {
        assert_eq!(clean_title("\"修复登录流程\""), "修复登录流程");
        assert_eq!(clean_title("“修复登录流程”"), "修复登录流程");
        assert_eq!(clean_title("修复登录流程\n\n第二行"), "修复登录流程 第二行");
        assert_eq!(clean_title("  "), "");
    }
}
