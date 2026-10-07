use serde::{Deserialize, Serialize};

/// Activation state of a single skill within a session.
///
/// Tracks whether a skill is currently loaded and available in the
/// agent's context window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillSessionEntryState {
    /// Skill is loaded and active in the current session.
    Active,
    /// Skill was previously available but is now unavailable
    /// (e.g. file deleted, scope changed).
    Unavailable,
}

/// Runtime tracking for one skill in a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillSessionEntry {
    /// Skill name matching SKILL.md metadata.
    pub name: String,
    /// Monotonic counter for determining activation order across sessions.
    pub activation_order: u64,
    /// Path or identifier of the skill source directory (project/user scope).
    pub source: String,
    /// Current activation state.
    pub state: SkillSessionEntryState,
}

/// Snapshot of skill activation state for a session, persisted in meta.json.
///
/// Version 2 adds `context_epoch` and `operation_revision` for tracking
/// skill activation/deactivation across context compaction cycles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillSessionStateV2 {
    /// Schema version (always 2).
    pub version: u8,
    /// Epoch counter incremented on context compaction. Used to detect
    /// whether stale skill contexts need refresh.
    pub context_epoch: u64,
    /// Monotonic revision counter for operation ordering across restarts.
    pub operation_revision: u64,
    /// Active skill entries in activation order.
    pub entries: Vec<SkillSessionEntry>,
}

impl Default for SkillSessionStateV2 {
    fn default() -> Self {
        Self {
            version: 2,
            context_epoch: 0,
            operation_revision: 0,
            entries: Vec::new(),
        }
    }
}

/// Session metadata — unified persistence + runtime state.
///
/// Fields marked `#[serde(skip)]` are runtime-only and not persisted to meta.json.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMeta {
    // ── Persisted fields ──
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// 会话选定的 BYOK profile（`config.toml` 的 `[profiles.<name>]`）。
    /// `None`/空 = 跟随全局 `active_profile`。它决定本会话的
    /// endpoint/model/wire，以及（若该 profile 自带密钥）用哪把 key。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub message_count: usize,
    /// Number of conversation turns (one user query + its assistant/tool chain).
    #[serde(default)]
    pub turn_count: usize,
    /// Number of earliest turns compacted (skipped in LLM context).
    #[serde(default)]
    pub compact_skip: usize,
    /// Highest archived `msg_id` covered by the latest compaction summary.
    ///
    /// `messages.jsonl` remains the immutable archive. On resume the active
    /// model view is derived as: leading system messages + the latest
    /// `[Compacted N turns]` message + every non-summary message with
    /// `msg_id > compact_covered_through_msg_id`. `None` means no compaction
    /// marker has been applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_covered_through_msg_id: Option<u64>,
    /// Agent operating mode: 0=Code(默认), 1=Plan, 2=Code(旧编码兼容).
    /// Persisted so PLAN/CODE mode survives agent restart within the same session.
    #[serde(default)]
    pub mode: u8,
    /// 工具模式：standard | minimal | custom（PLAN-TOOL-MODES.md）。
    /// 空串 = standard（旧 session 零迁移兼容）；custom 时 `custom_tools` 生效。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_mode: String,
    /// 创造模式的自定义工具白名单（仅 tool_mode == "custom" 时生效）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_tools: Vec<String>,
    /// 归档标记：标签 × 归档后置 true（会话停止、不出现在标签条，左侧
    /// 列表归档组可见可恢复）。旧 meta.json 缺失该字段 = 未归档。
    #[serde(default)]
    pub archived: bool,
    /// 临时会话标记（子代理）：`index=false` 写入时置 true。会话关闭时
    /// 整个目录被删除（用完即走，磁盘零残留）；正规会话恒为 false。
    /// 旧 meta.json 缺失该字段 = 非临时会话。
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub skills: SkillSessionStateV2,
    /// Frozen [Environment] annotation for the first user message (P0 cache
    /// fix). Generated once on the FIRST build_context() and reused for the
    /// lifetime of the session — persisting it keeps the provider prefix cache
    /// intact across daemon restarts / cross-day resumes: without persistence
    /// the annotation is regenerated on resume with a new <today> date and an
    /// empty file_state ledger, breaking the prefix at the first user message.
    /// 旧 meta.json 缺失该字段 = None = 恢复后首次 build_context 重新生成
    /// （即修复前的行为，零迁移兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_annotation: Option<String>,
    /// Provider-confirmed usage accumulated across model requests in this session.
    #[serde(default)]
    pub usage_totals: crate::UsageInfo,
    /// Last provider-confirmed request usage, used to restore the live Info panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<crate::UsageInfo>,
    /// Number of model requests included in `usage_totals`.
    #[serde(default)]
    pub usage_requests: u32,
    /// Number of requests whose provider explicitly returned cache usage.
    #[serde(default)]
    pub cache_reported_requests: u32,

    // ── Runtime fields (not persisted) ──
    /// If set, this seed is passed as a CLI argument to the agent subprocess for auto-restore on startup.
    #[serde(skip)]
    pub resume_session: Option<String>,
    /// Cumulative tokens consumed across all turns.
    #[serde(skip)]
    pub tokens: u64,
    /// 会话标题（**首轮后生成一次即冻结**，对齐主流 AI 工具行为；persisted）。
    /// 生成链路：worker 首 turn 完成后异步 LLM 总结用户需求（失败降级为
    /// 首条用户消息截断）→ 写盘 → daemon 广播 `SessionMetaChanged` → 前端刷新。
    pub title: Option<String>,
    /// 会话创建时的工作目录（canonical path，persisted）。Workspace 归属判定
    /// 基础：新会话 cwd 位于某 workspace path 内自动 attach；旧 meta.json 缺省
    /// None = 未分组（零迁移兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 上下文统计快照（可再生缓存：compact/dashboard 时重算）。原独立文件
    /// `sessions/{seed}/context_stats.json` 已退役，并入 meta.json。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_stats: Option<serde_json::Value>,
    /// True if session was restored from disk — system prompt preserved.
    #[serde(skip)]
    pub from_resume: bool,
}
impl SessionMeta {
    pub fn effective_cache_reported_requests(&self) -> u32 {
        if self.cache_reported_requests == 0
            && self.usage_requests > 0
            && self
                .usage_totals
                .prompt_cache_hit_tokens
                .saturating_add(self.usage_totals.prompt_cache_miss_tokens)
                > 0
        {
            self.usage_requests
        } else {
            self.cache_reported_requests
        }
    }

    pub fn record_usage(&mut self, usage: &crate::UsageInfo) {
        self.cache_reported_requests = self.effective_cache_reported_requests();
        if self.cache_reported_requests > 0 {
            self.usage_totals.cache_usage_reported = Some(true);
        }
        self.usage_totals.prompt_tokens = self
            .usage_totals
            .prompt_tokens
            .saturating_add(usage.prompt_tokens);
        self.usage_totals.completion_tokens = self
            .usage_totals
            .completion_tokens
            .saturating_add(usage.completion_tokens);
        self.usage_totals.total_tokens = self
            .usage_totals
            .total_tokens
            .saturating_add(usage.total_tokens);
        self.usage_totals.prompt_cache_hit_tokens = self
            .usage_totals
            .prompt_cache_hit_tokens
            .saturating_add(usage.prompt_cache_hit_tokens);
        self.usage_totals.prompt_cache_miss_tokens = self
            .usage_totals
            .prompt_cache_miss_tokens
            .saturating_add(usage.prompt_cache_miss_tokens);
        self.usage_totals.reasoning_tokens = self
            .usage_totals
            .reasoning_tokens
            .saturating_add(usage.reasoning_tokens);
        if usage.cache_usage_reported == Some(true) {
            self.usage_totals.cache_usage_reported = Some(true);
        }
        self.usage_requests = self.usage_requests.saturating_add(1);
        if usage.cache_usage_reported == Some(true) {
            self.cache_reported_requests = self.cache_reported_requests.saturating_add(1);
        }
        self.last_usage = Some(usage.clone());
        self.tokens = self.usage_totals.total_tokens.into();
    }

    pub fn reset_usage(&mut self) {
        self.tokens = 0;
        self.usage_totals = crate::UsageInfo::default();
        self.last_usage = None;
        self.usage_requests = 0;
        self.cache_reported_requests = 0;
    }

    /// 会话列表/tab 的**展示标题**（前端契约 **G2** 定死的口径）：
    /// `title` → `cwd` 尾段 → `session_id`。
    ///
    /// 标题只有两个来源（2026-10-06 归一裁决）：worker 首 turn 后 LLM 生成，
    /// 或生成失败时回退的首条用户消息截断（`FALLBACK_MAX_CHARS`）。不存在
    /// 「最后一条回复」类的漂移来源——列表标题不随对话内容变化。
    pub fn display_title(&self) -> String {
        if let Some(title) = self.title.as_deref().filter(|s| !s.is_empty()) {
            return title.to_owned();
        }
        if let Some(cwd) = self.cwd.as_deref().filter(|s| !s.is_empty()) {
            // 去掉尾部分隔符后取最后一段（两种分隔符都吃：cwd 可能是 Windows 路径）。
            // 用 `rsplit_once` 而非按字节下标切——后者在 UTF-8 边界上会 panic
            // （本 crate 的 clippy 配置 `-D clippy::string-slice`）。
            let trimmed = cwd.trim_end_matches(['/', '\\']);
            return match trimmed.rsplit_once(['/', '\\']) {
                Some((_, tail)) => tail.to_owned(),
                None => trimmed.to_owned(),
            };
        }
        self.session_id.clone()
    }
}

/// `session.list` / `session.meta` 条目的**统一运行状态词表**（前端契约 **G2**）。
///
/// 2026-10-06 归一裁决：废除「worker 进程存在性」（旧 `running: bool`）语义，
/// 会话当前状态一律用 agentloop 状态表达。权威来源是 **canonical fact 投影**
/// （control 频道 activity + 挂起交互 + conversation 最近回合终态）——它从
/// 持久 fact 重放，daemon 重启后可重建；live tracker 只作投影不可读时的退路。
///
/// 驻留语义：`canceled` / `error` 是**最近一个回合的终态**，驻留到下一回合
/// 开始（`TurnStarted` 后进入 `working`）；`idle` 表示 loop 空闲等待输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum SessionRunStatus {
    /// daemon 本进程内没有该会话的 worker：不读投影、不做任何合成。
    /// 未加载 ≠ idle——前端不得把本状态渲染成「空闲」。
    #[default]
    NotRunning,
    Idle,
    Working,
    /// 等待工具授权（interaction kind = permission）。
    WaitingPermission,
    /// 等待用户回答 ask。
    WaitingAsk,
    /// 等待计划评审。
    WaitingPlan,
    /// 最近一个回合被取消（terminal = cancelled，或 interrupt reason =
    /// cancel_before_seal）。
    Canceled,
    /// 最近一个回合失败或被非用户原因打断（terminal = failed，interrupt
    /// reason = crash / restart / unknown_fact）。
    Error,
}

/// `session.list` 的条目 = [`SessionMeta`] + daemon 运行期附加字段（前端契约 **G2**）。
///
/// 此前这个形状只活在 `qaqh-runtime` 的 `serde_json::Value` 拼装里
/// （`to_value(&meta)` 之后再 `value["running"] = …`），三端前端只能各自手解。
/// TUI 为此维护了 128 行手抄，而 `created_at` / `turn_count` / `message_count` /
/// `tool_mode` 这几个键**它一个都没解**（`grep` 逐个为 0）——漏了没人发现，
/// 因为手抄的失败模式是**静默的**：漏字段不报错，只让某个功能永远显示缺省值。
///
/// **加法式**：`session.list` 回包仍是同一个 JSON 对象（[`SessionMeta`] 的键经
/// `flatten` 平铺，外加 `status` / `workspace_id`），wire 未变。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionListEntry {
    /// 持久化元数据。`flatten` 让它在 wire 上与下面两个运行期字段**同层**
    /// （历史形状如此，不能改成嵌套）。
    #[serde(flatten)]
    pub meta: SessionMeta,
    /// 统一运行状态（[`SessionRunStatus`]）——daemon 投影/registry 的**实时**
    /// 查询结果，不落盘，故不属于 [`SessionMeta`]。取代旧 `running: bool`
    /// （worker 进程存在性语义已于 2026-10-06 废除）。
    #[serde(default)]
    pub status: SessionRunStatus,
    /// 所属 workspace id；`null` = 未分组。
    /// 只有 `session.list` 带此键，`session.meta`（单条）不带。
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个可序列化字段都非缺省的 meta——用于 wire 契约锁。
    fn fully_populated_meta() -> SessionMeta {
        SessionMeta {
            session_id: "0123abcd".into(),
            created_at: 1,
            updated_at: 2,
            model: "m1".into(),
            effort: Some("high".into()),
            profile: Some("deep".into()),
            message_count: 3,
            turn_count: 4,
            compact_skip: 5,
            compact_covered_through_msg_id: Some(9),
            mode: 1,
            tool_mode: "custom".into(),
            custom_tools: vec!["bash".into()],
            archived: true,
            ephemeral: true,
            skills: SkillSessionStateV2::default(),
            frozen_annotation: Some("<today>2026-09-15</today>".into()),
            usage_totals: crate::UsageInfo::default(),
            last_usage: Some(crate::UsageInfo::default()),
            usage_requests: 6,
            cache_reported_requests: 7,
            title: Some("Bun 引导 daemon".into()),
            cwd: Some("F:\\code\\qaqh".into()),
            context_stats: Some(serde_json::json!({ "tokens": 1 })),
            resume_session: Some("skip-me".into()),
            tokens: 8,
            from_resume: true,
        }
    }

    /// **G2 回归闸（产出方契约）**：`session.list` 的条目形状。
    ///
    /// 这份键表是**手工维护的 wire 契约**——不是从类型推导出来的（那样就成了
    /// 同义反复）。`SessionMeta` 增删字段必须同步改这里，这正是目的：让每一次
    /// 形状变更都显式过一次评审，而不是悄悄漂走。
    #[test]
    fn session_list_entry_wire_keys_are_locked() {
        let entry = SessionListEntry {
            meta: fully_populated_meta(),
            status: SessionRunStatus::Working,
            workspace_id: Some("w1".into()),
        };
        let wire = serde_json::to_value(&entry).expect("serialize");
        let mut keys: Vec<&str> = wire
            .as_object()
            .expect("条目必须是对象（flatten 不得改成嵌套）")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();

        let mut expected = vec![
            // ── SessionMeta 的持久化字段（全量，含手抄时代漏掉的那些）──
            "archived",
            "cache_reported_requests",
            "compact_covered_through_msg_id",
            "compact_skip",
            "context_stats",
            "created_at",
            "custom_tools",
            "cwd",
            "effort",
            "ephemeral",
            "frozen_annotation",
            "last_usage",
            "message_count",
            "mode",
            "model",
            "profile",
            "session_id",
            "skills",
            // ── 运行期附加字段 ──
            "status",
            "title",
            "tool_mode",
            "turn_count",
            "updated_at",
            "usage_requests",
            "usage_totals",
            "workspace_id",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected, "session.list 条目的 wire 键集合变了");

        // 运行期字段**不落 wire**：`#[serde(skip)]` 掉了就必须一直是掉的。
        for runtime_only in ["resume_session", "tokens", "from_resume"] {
            assert!(
                !wire.as_object().unwrap().contains_key(runtime_only),
                "{runtime_only} 是运行期字段，不得出现在 session.list 里"
            );
        }
    }

    /// **G2 回归闸（消费方需求）**：类型化后逐字段可达，且往返无损。
    ///
    /// 断言刻意贴着「前端真正要用的那几个」写——TUI 手抄件当年只解出这一小撮，
    /// 其余全部漏掉；这份测试保证漏掉的那些**在类型上够得着**（而不是逼前端
    /// 回头去 `value.get("…")`）。
    #[test]
    fn session_list_entry_recovers_fields_the_hand_parse_dropped() {
        let entry = SessionListEntry {
            meta: fully_populated_meta(),
            status: SessionRunStatus::Working,
            workspace_id: Some("w1".into()),
        };
        let wire = serde_json::to_value(&entry).expect("serialize");
        let back: SessionListEntry = serde_json::from_value(wire.clone()).expect("deserialize");

        // 手抄时代一个都没解的字段（grep 逐个为 0）。
        assert_eq!(back.meta.created_at, 1);
        assert_eq!(back.meta.turn_count, 4);
        assert_eq!(back.meta.message_count, 3);
        assert_eq!(back.meta.tool_mode, "custom");
        // 连带的其余持久化字段同样够得着。
        assert_eq!(back.meta.compact_skip, 5);
        assert_eq!(back.meta.custom_tools, vec!["bash".to_string()]);
        assert_eq!(back.meta.usage_requests, 6);
        assert_eq!(back.meta.cache_reported_requests, 7);

        // 运行期字段是**类型上的字段**，不再靠 `value["running"]`。
        assert_eq!(back.status, SessionRunStatus::Working);
        assert_eq!(back.workspace_id.as_deref(), Some("w1"));

        // 往返无损（flatten 下 Option/skip_serializing_if 语义不得变）。
        assert_eq!(serde_json::to_value(&back).unwrap(), wire);

        // 未分组会话（无 workspace）仍须带键——历史形状是 `null`，不是缺键。
        let entry = SessionListEntry {
            meta: fully_populated_meta(),
            status: SessionRunStatus::NotRunning,
            workspace_id: None,
        };
        let wire = serde_json::to_value(&entry).unwrap();
        assert!(
            wire.as_object().unwrap().contains_key("workspace_id")
                && wire["workspace_id"].is_null(),
            "未分组必须是 `null`（历史形状如此），不是缺键"
        );
    }

    /// 统一状态词表在 wire 上是 snake_case 字符串，且缺省 = `not_running`
    /// （旧回包缺 `status` 键时不得被误读成 idle）。
    #[test]
    fn session_run_status_wire_vocabulary_is_locked() {
        assert_eq!(
            serde_json::to_value(SessionRunStatus::WaitingPermission).unwrap(),
            serde_json::json!("waiting_permission")
        );
        assert_eq!(
            serde_json::to_value(SessionRunStatus::NotRunning).unwrap(),
            serde_json::json!("not_running")
        );
        assert_eq!(
            serde_json::from_value::<SessionRunStatus>(serde_json::json!("waiting_plan")).unwrap(),
            SessionRunStatus::WaitingPlan
        );
        // 没有 `#[serde(other)]` 兜底臂：未知取值必须**响亮地失败**而不是
        // 静默降级成一个错的状态（对齐 InteractionKind 的决策记录）。
        assert!(serde_json::from_value::<SessionRunStatus>(serde_json::json!("running")).is_err());
    }

    /// 展示标题口径（G2 一并定死，三端共用）。
    #[test]
    fn display_title_prefers_title_then_cwd_tail_then_session() {
        let mut meta = SessionMeta {
            session_id: "0123abcd".into(),
            title: Some("Bun 引导 daemon".into()),
            cwd: Some("/home/me/proj".into()),
            ..Default::default()
        };
        assert_eq!(meta.display_title(), "Bun 引导 daemon");

        // title 缺失 → cwd 尾段（Windows 分隔符同样吃）。
        meta.title = Some(String::new());
        assert_eq!(meta.display_title(), "proj");
        meta.cwd = Some("F:\\code\\qaqh\\".into());
        assert_eq!(meta.display_title(), "qaqh");
        meta.cwd = Some("qaqh".into());
        assert_eq!(meta.display_title(), "qaqh");

        // 都没有 → session_id。
        meta.cwd = None;
        assert_eq!(meta.display_title(), "0123abcd");
    }

    #[test]
    fn session_meta_serializes_session_id() {
        let meta: SessionMeta = serde_json::from_str(
            r#"{"session_id":"s-1","created_at":1,"updated_at":1,"model":"m","message_count":0}"#,
        )
        .expect("session meta must deserialize");
        assert_eq!(meta.session_id, "s-1");

        let wire = serde_json::to_value(&meta).expect("serialize");
        assert_eq!(wire["session_id"], "s-1");
        assert!(wire.get("seed").is_none(), "seed must not be emitted");
    }

    #[test]
    fn legacy_session_metadata_defaults_to_empty_skill_state_v2() {
        let meta: SessionMeta = serde_json::from_str(
            r#"{
            "session_id":"s","created_at":0,"updated_at":0,"model":"m",
            "message_count":0,"turn_count":0,"last_summary":"","compact_skip":0,"mode":0
        }"#,
        )
        .unwrap();
        assert_eq!(meta.skills.version, 2);
        assert!(meta.skills.entries.is_empty());
        assert_eq!(meta.cache_reported_requests, 0);
    }

    #[test]
    fn legacy_session_metadata_defaults_tool_mode_to_standard() {
        // 旧 meta.json 无 tool_mode/custom_tools → 零迁移兼容（standard）。
        let meta: SessionMeta = serde_json::from_str(
            r#"{
            "session_id":"s","created_at":0,"updated_at":0,"model":"m",
            "message_count":0,"turn_count":0,"last_summary":"","compact_skip":0,"mode":1
        }"#,
        )
        .unwrap();
        assert_eq!(meta.tool_mode, "");
        assert!(meta.custom_tools.is_empty());
    }

    #[test]
    fn tool_mode_round_trips_through_json() {
        let meta = SessionMeta {
            tool_mode: "custom".to_string(),
            custom_tools: vec!["bash".to_string(), "edit".to_string()],
            ..Default::default()
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_mode, "custom");
        assert_eq!(back.custom_tools, vec!["bash", "edit"]);
        // standard 时空 custom_tools 不落盘（skip_serializing_if）
        let meta2 = SessionMeta {
            tool_mode: "minimal".to_string(),
            ..Default::default()
        };
        let json2 = serde_json::to_string(&meta2).unwrap();
        assert!(!json2.contains("custom_tools"));
    }

    #[test]
    fn usage_tracks_cache_reporting_separately_from_hit_rate() {
        let mut meta = SessionMeta::default();
        meta.record_usage(&crate::UsageInfo {
            prompt_tokens: 100,
            prompt_cache_miss_tokens: 100,
            cache_usage_reported: Some(true),
            ..Default::default()
        });
        meta.record_usage(&crate::UsageInfo {
            prompt_tokens: 50,
            ..Default::default()
        });

        assert_eq!(meta.usage_requests, 2);
        assert_eq!(meta.cache_reported_requests, 1);
        assert_eq!(meta.usage_totals.cache_usage_reported, Some(true));
        assert_eq!(meta.usage_totals.prompt_cache_hit_tokens, 0);
        assert_eq!(meta.usage_totals.prompt_cache_miss_tokens, 100);
    }

    #[test]
    fn frozen_annotation_round_trips_and_legacy_meta_defaults_to_none() {
        // 旧 meta.json 无 frozen_annotation → None（零迁移，恢复后重新生成）。
        let legacy: SessionMeta = serde_json::from_str(
            r#"{
            "session_id":"s","created_at":0,"updated_at":0,"model":"m","message_count":0
        }"#,
        )
        .unwrap();
        assert_eq!(legacy.frozen_annotation, None);

        // 有值时 round-trip 保真；None 时不落盘（skip_serializing_if）。
        let meta = SessionMeta {
            frozen_annotation: Some(
                "<workspace_path>F:\\proj</workspace_path>\n<today>2026-09-10</today>".into(),
            ),
            ..Default::default()
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("frozen_annotation"));
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.frozen_annotation, meta.frozen_annotation);

        let none_json = serde_json::to_string(&SessionMeta::default()).unwrap();
        assert!(!none_json.contains("frozen_annotation"));
    }

    #[test]
    fn legacy_cache_totals_infer_full_request_coverage() {
        let meta = SessionMeta {
            usage_requests: 3,
            usage_totals: crate::UsageInfo {
                prompt_cache_hit_tokens: 60,
                prompt_cache_miss_tokens: 40,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(meta.effective_cache_reported_requests(), 3);
    }
}
