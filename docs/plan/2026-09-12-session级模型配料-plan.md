# Session 级模型配料（per-session model override）— 实施计划（2026-09-12）

> 状态：**草案待评审**。本文是 `docs/plan/` 意义上的实施计划：背景、决策记录、分阶段任务、
> 验收标准、风险清单。落地过程如与本文冲突，先改本文再改代码。
> 关联调研结论见同日对话摘要；参考设计：`D:\project\deepseek-harness-master`
> （`packages/api/session-controller` model-selection 体系、`packages/core/agent-default-model`、
> `packages/client/ui-model-selection`）。

## 0. 元信息

| 项 | 值 |
|---|---|
| 计划日期 | 2026-09-12（UTC+8） |
| 涉及仓库 | 后端 `D:\project\QAQ-Harness`；前端 `D:\project\qaqh-winui-app` |
| 行号基准 | 后端 `4e03a88`（仅调研未改码）；前端为当日工作区 |
| 执行者 | 待定（后端 runtime/config/session 各 1 人日级改动 × 3 期；前端 1–2 人日） |
| 一句话目标 | 让每个会话可固定自己的 (provider, endpoint, model, effort) 配料；未固定会话跟随全局默认；**任何全局配置变更或会话切换不得改变已 pin 会话的请求路由（防串频）** |

## 1. 背景与问题陈述

### 1.1 现状缺陷（调研结论，均有代码锚点）

1. **模型配置是全局单值**：`config.toml` 只有一份 `provider_id/endpoint/model/base_url/api_key`
   （`qaqh-config/src/config.rs` `PersistentConfig`/`Config`）。
2. **全局保存热覆写所有活跃会话**：`config.save` → `notify_config_changed`
   （`qaqh-runtime/src/service.rs:736-743`）→ `broadcast_ringing(AgentReloadConfig)`
   → 各 worker `SessionEngine::apply_config`（`qaqh-runtime/src/agent/engine_session.rs:96-114`）
   **整体拷贝** model/base_url/api_key/provider_id/endpoint 到运行中 `agent.config`。
3. **请求路由按回合现场解析**：`turn_lap/gate.rs:713-794 provider_for` 每回合从
   `agent.config` 重建 `ProviderConfig`（含工具返回后的续轮请求，`engine_turn.rs:1008`）。
   → 串频场景成立：sessionA 用 deepseek 期间全局切到 GLM，A 的下一请求（含续轮）发往 GLM。
4. **worker 重建也丢会话语义**：`agent/spawn.rs:83` 每次 spawn 都读全局
   `qaqh_config::watch::authoritative()`；空闲卸载后拉起同样回到全局值。
5. **meta.model 只是展示记录**（`qaqh-message/src/store.rs:299 flush_meta`），
   resume 时不读取、不参与路由（`qaqh-session/src/manager.rs`；`state/lifecycle.rs`）。
6. **前端无会话级选择入口**：模型设置只有设置页全局 `config.save`
   （`qaqh-winui-app/apps/winui/src/settings_view/view.rs:176-209`）与 `profile.apply`；
   composer 的 `/model`（`composer_bar/mod.rs:150`）是死条目。

### 1.2 参照系：DSH 的设计原则（本文采纳）

| 原则 | DSH 佐证 | 本文落点 |
|---|---|---|
| 会话配料是会话的事实，持久化在会话自身 | session log `model/selection` 事件 + `modelSelection` 投影（`model-selection-projection.ts`） | `SessionMeta.model_override`（meta.json 按 seed 隔离） |
| 全局默认独立存在，仅作缺省 | `agent-default-model` settings namespace（`packages/core/agent-default-model`） | `config.toml` 保持为默认值；`apply_config` 改合并语义 |
| 切换=定向作用于该会话，请求时现场解析 | agent scope 内 `agent/request` waterfall 覆写 `LlmCallConfig`（`model-selection.ts`） | 定向 `ControlCommand::SetSessionModel` + worker 覆写自身 config；`provider_for` 无需改动 |
| 入口校验 + 切换告知模型 | `resolveCallConfig` 校验；`modelSwitchNotice` user-role 通知 | `session.set_model` 校验 registry；切换插入通知消息 |
| 前端 per-session 目录 + 单一提交 RPC | `ModelDirectoryResolver`/`ModelDirectory`/`ModelSelect.tsx` | composer 模型 chip + `session.set_model` / `session.clear_model` |

### 1.3 非目标（明确不做）

- 不引入跨进程事件日志/投影基建（QAQ 的 meta.json + 定向命令已等价满足）。
- 不做 turn 内即时熔断切换：pin 生效于**下一回合/下一 lap**（与 DSH 语义一致）。
- 不支持会话级 base_url/api_key 自由编辑：这两项由 provider/endpoint 派生
  （key 走 secrets 槽），避免把凭据写进 meta.json。
- 不改 subagent 配料机制（已有 `[subagent]` 配置 + spawn 覆写，保持现状）。

## 2. 关键决策记录（需评审确认）

| ID | 决策 | 理由 | 备选与放弃原因 |
|---|---|---|---|
| D1 | 配料存 `SessionMeta.model_override: Option<SessionModelOverride>`（新结构：`provider_id/endpoint/model/effort`） | meta.json 已按 seed 隔离、原子写、resume 既有通路；serde(default) 零迁移 | 存独立 `model.txt`：多一个文件面；存 messages.jsonl 系统 marker：污染模型历史 |
| D2 | key 不存 meta，运行时按 provider 解析 secrets；`SecretSlot` 扩展 `Provider(String)` | DSH 同样把凭据与选择分离；避免明文/换 key 不同步 | meta 存 key：安全红线；每会话一份 base_url：registry 已是权威 |
| D3 | wire 用一条定向命令 `ControlCommand::SetSessionModel`（daemon 先落 meta 再发命令），与 `SetToolMode` 同构 | 复用 CK-PERSIST 纪律与 keyed 定向路由；broadcast 被结构性排除 | 复用 `AgentReloadConfig`+worker 读 meta：会引入"谁读了、没读"竞态 |
| D4 | 全局 reload 对 pinned 会话**跳过路由五字段**（provider/endpoint/base_url/model/effort），其余字段照旧热同步 | 防串频的最后一道闸；非路由字段（compact 阈值、权限）仍需跟随全局 | 全字段跳过：权限等安全字段不跟随不可接受 |
| D5 | 切换通知消息走既有注入管线（skills envelope 同款），文本 `[model changed: …]`（对齐 DSH 文案） | 模型需知上文由谁生成；复用投影规则避免破坏 provider 兼容性 | 裸 push_system：system 消息在部分端点的 history 位置语义不稳 |
| D6 | effort 语义：`Some(显式)`=钉死；`None`=跟随该端点默认 | 与 DSH `reasoningEffort` 语义一致；UI 可提供"端点默认"项 | 去掉会话级 effort：用户强诉求（deepseek max vs glm high） |

## 3. 交付物总览（分三期）

```text
P1 后端闭环（防串频先落地）
  ① qaqh-types: SessionMeta.model_override + SessionModelOverride
  ② qaqh-session: persist_model_override / clear_model_override（with_meta_locked）
  ③ qaqh-domain: ControlCommand::SetSessionModel / ClearSessionModel
  ④ qaqh-runtime service: session.set_model / session.clear_model action
     （校验 registry → 落 meta → 定向 send_ringing_cmd）
  ⑤ worker: apply_session_model + 全局 reload 合并守卫（D4）
P2 恢复与目录
  ⑥ lifecycle: init_session / create_session 应用 meta.model_override
  ⑦ qaqh-config: SecretSlot::Provider(id) + resolve_for_provider
  ⑧ service: session.model_catalog 查询
P3 前端
  ⑨ qaqh-client: ActionRequest 三个新变体 + client 重导出
  ⑩ WinUI bridge: spawn_session_set_model / clear / catalog + per-seed 目录缓存
  ⑪ composer 模型 chip（双态渲染 + Flyout 选择器）+ 设置页文案
  ⑫ /model 斜杠命令接入或移除
```

## 4. 分阶段任务明细

### P1-① `qaqh-types/src/session.rs`

- `SessionMeta` 增加字段：

```rust
/// 会话级配料覆盖（pin）。None = 跟随全局默认；旧 meta 缺字段 = None。
#[serde(default, skip_serializing_if = "Option::is_none")]
pub model_override: Option<SessionModelOverride>,
```

- 新类型：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModelOverride {
    pub provider_id: String,
    pub endpoint: String,
    pub model: String,
    /// None = 跟随端点默认 effort。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}
```

- 验收：旧 meta.json 反序列化成功；`session.list`/`session.meta` 自动带出该字段。

### P1-② `qaqh-session/src/manager.rs`

- 仿照 `persist_tool_mode`（L486-509）实现：

```rust
pub fn persist_model_override(&self, seed: &str, o: &SessionModelOverride) -> Result<(), String>;
pub fn clear_model_override(&self, seed: &str) -> Result<(), String>;
```

- 两者都必须 `with_meta_locked(seed, true, …)` + `upsert_index`，写失败向上返回
  （CK-PERSIST 纪律，禁止"应用成功落盘失败"）。
- 验收：写后 `load_meta` 读回一致；并发写不损坏 meta（复用 session_lock 既有测试范式）。

### P1-③ `qaqh-domain/src/command.rs`

```rust
/// 会话级模型切换（daemon 已落 meta；worker 只应用，与 SetToolMode 同构）。
SetSessionModel {
    provider_id: String,
    endpoint: String,
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
},
/// 清除会话级配料，回到全局默认。
ClearSessionModel,
```

- 两个变体**不是** interrupt 类（不进 `ringing_command_is_interrupt`）。
- 验收：serde 往返；`feature = "ts"` 导出更新（如有 ts 产物流水线需同步）。

### P1-④ `qaqh-runtime/src/service.rs`

- dispatch 新增 action：

```text
session.set_model { seed, provider_id, endpoint, model, effort? }
  1. registry::find_endpoint(provider_id, endpoint) 不存在 → 400（fail-fast）
  2. model 为空 → 400
  3. sessions.persist_model_override(...) 失败 → 400
  4. send_ringing_cmd(seed, ControlCommand::SetSessionModel{..}) —— 定向，绝不 broadcast
session.clear_model { seed }
  1. sessions.clear_model_override(...) 失败 → 400
  2. send_ringing_cmd(seed, ControlCommand::ClearSessionModel)
```

- **红线**：两个 action 的实现路径上不得出现 `broadcast_ringing`；
  用 `rg -n "broadcast" 涉及函数` 在 code review 时检查。
- 验收：`session.set_model` 对不存在的 seed 返回明确错误；对运行中 worker 会话
  200 后 worker 日志出现应用记录。

### P1-⑤ worker 应用 + 全局 reload 合并守卫（防串核心）

- `loop_dispatch_control.rs` `on_control` 增加两分支：
  - `SetSessionModel` → `agent.apply_session_model(override)` → `emit_operation_completed`；
  - `ClearSessionModel` → `agent.clear_session_model()`（恢复全局五字段 + 清
    `session.model_override`）。
- `agent.rs` 新增：

```rust
pub fn apply_session_model(&mut self, o: SessionModelOverride) {
    // provider/endpoint → base_url（registry 权威），key → secrets 槽（P2 前先沿用
    // find_endpoint 的端点 + 现有 main key，P2 落 per-provider key 后切换解析源），
    // model/effort 直写；随后 refresh_endpoint_spec + refresh_image_capability。
}
```

  注意：`refresh_image_capability` 必须调用（新 provider 可能开启 `read_image`）。
- **`engine_session.rs apply_config` 合并守卫（D4）**：

```rust
let pinned = agent.session.model_override.is_some();
if !pinned {
    agent.config.provider_id = cfg.provider_id;
    agent.config.endpoint    = cfg.endpoint;
    agent.config.base_url    = cfg.base_url;
    agent.config.model       = cfg.model;
    agent.config.reasoning_effort = cfg.reasoning_effort;
}
```

  同步更新守卫测试 `applies_all_hot_fields`（engine_session.rs L123-164）：
  pinned 场景断言五字段保持旧值、其余热字段仍更新。
- 验收（P1 出口标准，全部自动化）：
  1. `apply_config` 单测：pinned 会话在全局 model/base_url 变化后 `agent.config.model`
     不变；
  2. 集成测试（模板 `qaqh-runtime/tests/config_save_api_key_preserve.rs`）：
     两个 seed，A pin GLM、B 不 pin；`config.save` 改全局 model；断言
     `provider_for` 产物——A 用 GLM 端点、B 用新全局值；
  3. 手动：A 跑长工具任务中途全局切模型，A 续轮请求端点不变（`run_lib.log`/`qaqh-daemon.log` 佐证）。

### P2-⑥ `state/lifecycle.rs`

- `init_session`（resume 成功分支，`agent.session = meta` 之后）与
  `create_session` / `create_session_with_seed`（meta 预置分支，对齐
  `create_session_with_seed` 已有的 tool_mode 预置读法 L330-337）：
  `if let Some(o) = &meta.model_override { agent.apply_session_model(o.clone()); }`
- 位置约束：必须在 `refresh_endpoint_spec` 生效前、首轮 `flush_meta` 之前。
- 验收：resume 后 `agent.config.model == override.model`；无 override 会话回归不变
  （现有 session_lifecycle 测试通过）。

### P2-⑦ `qaqh-config/src/secrets.rs`

- `SecretSlot` 增加 `Provider(String)`；`key()` 返回 `providers/{id}`（serde 层用
  新表 `[providers.{id}]`，与现有 `main/subagent` 槽并列）。
- 兼容迁移（一次性，daemon 启动时）：`main` 槽有值且当前 provider 槽为空 →
  复制到 `providers/{当前 provider_id}`；原槽保留（回滚安全）。
- 验收：迁移幂等；DPAPI 加解密往返；`has/load/set/delete` 四路单测。

### P2-⑧ `session.model_catalog`

- 返回结构（JSON）：

```json
{
  "default": { "providerId": "deepseek", "endpoint": "openai", "model": "deepseek-chat" },
  "providers": [{
    "id": "zhipu", "display": "GLM", "routable": true,
    "endpoints": [{ "id": "openai", "display": "OpenAI-compatible",
                    "baseUrl": "…", "models": ["glm-5", "glm-5-air"],
                    "supportsThinking": true }]
  }]
}
```

- 数据源：`qaqh_config::registry::merged()`（公开 merged 快照或新增只读迭代 API）+
  `SecretStore::has(Provider(id))` 决定 `routable`。
- 验收：13 个 baseline provider 均出现；override 段生效（base_url 覆盖可见）。

### P3-⑨ `qaqh-client`

- `ActionRequest` 新增 `SessionSetModel {..} / SessionClearModel {..} / SessionModelCatalog`，
  映射到对应 method 字符串；`bridge/types.rs` 同步。

### P3-⑩⑪ WinUI

- `bridge/core_sessions.rs`：`spawn_session_set_model/clear`（提交时点锁
  `active_seed()`，注释纪律同 `spawn_send_message` L284-286）；目录缓存
  `Mutex<HashMap<seed, ModelDirSnapshot>>`，会话关闭/删除时清条目。
- composer 状态条新增模型 chip（与权限 chip/工具模式/执行规划同排）：
  - 未 pin：显示当前生效值（来自 `session.meta.model_override ?? config`）；
  - 已 pin：accent 描边 + "已固定"角标；
  - 点击 Flyout：provider 分组 → model 列表 →（可选）effort 子列表
    （`supportsThinking` 才显示 effort 组，含"端点默认"项）+ "跟随全局默认"项（发 clear）；
  - 失败 toast，成功乐观更新本地快照（以 `session.meta` 刷新为准校正）。
- 设置页"模型"分区顶部加说明：**全局默认**与**会话内固定**的关系。
- `/model` 斜杠命令：接入 chip 弹出或从 `SLASH_COMMANDS` 移除（二选一，不留死条目）。

### P3-⑫ 前端验收

1. 会话 A pin GLM、会话 B pin deepseek，来回切换 tab，chip 各显示各的；
2. A 选 GLM 后立刻在 A 发消息 → `qaqh-daemon.log` 中该 turn 请求 URL 为 GLM 端点；
3. B 会话全程不受影响（日志端点不变）；
4. 设置页全局切换后：A/B 均不变（pinned），新建会话用新全局默认；
5. A 空闲卸载再唤醒（或重启 daemon 恢复）后 chip 仍为 GLM、请求端点正确。

## 5. 测试与回归清单（汇总）

| 层 | 用例 | 类型 |
|---|---|---|
| qaqh-types | 旧 meta 反序列化（无 model_override 字段）→ None | 单测 |
| qaqh-session | persist/clear 往返 + 并发写 | 单测 |
| qaqh-domain | 新命令 serde 往返 | 单测 |
| qaqh-runtime | `applies_all_hot_fields` 更新：pinned 不覆盖五字段、其余热字段照常 | 单测（守卫） |
| qaqh-runtime | 双会话全局 reload 隔离（P1 出口标准 2） | 集成 |
| qaqh-runtime | resume 应用 override | 集成 |
| qaqh-config | SecretSlot::Provider 迁移幂等 + DPAPI 往返 | 单测 |
| qaqh-config | model_catalog 结构与 override 生效 | 单测 |
| 前端 | 手动验收清单 P3-⑫ | 手动 |

## 6. 风险与开放问题

| ID | 风险/问题 | 缓解 | 状态 |
|---|---|---|---|
| R1 | key 槽迁移丢凭据 | 迁移只复制不删除原槽；DPAPI 往返单测；发布说明提示 | 开放：评审确认迁移时机 |
| R2 | turn 进行中切换产生"半 turn 换模型" | 语义定为"下一 lap 生效"+ 通知消息；文档写明 | 已决策（§1.3） |
| R3 | 通知消息被部分端点拒绝 | 走既有注入管线（与 skills envelope 同风险面）；先在 deepseek/glm/qwen 三端点实测 | 开放：P1 验收含实测 |
| R4 | `session.set_model` 与并行 `session.resume` 竞态（worker 未起时命令排队） | `send_ringing_cmd` 既有 get_or_spawn 语义保证命令最终送达 actor；meta 已先落盘，actor 起始时读 meta 兜底 | 已覆盖 |
| R5 | 前端 catalog 与后端 registry 版本漂移（override 热重载） | chip 打开时强制重拉 `session.model_catalog`；后端 `invalidate_merged` 后 rev 推送已有 | 已覆盖 |
| R6 | profile 体系与新 pin 的交互（pin 引用 profile 还是四元组） | 本期 pin 存四元组；profile.apply 只改全局默认。是否支持"会话 pin 绑定 profile 名"留待用户反馈 | 开放 |
| R7 | effort 钉死后 provider 换端点导致非法 effort | `apply_session_model` 时按端点 `supports_reasoning_effort`/`thinking_budget_large` 校验，非法则清 effort 并记录日志 | 已覆盖 |

## 7. 排期建议

| 期 | 工作量估算 | 出口标准 |
|---|---|---|
| P1 | 后端 5 文件 + 3 测试，约 1 人日 | §4 P1 验收 1–3 全绿 |
| P2 | 后端 3 文件 + 2 测试，约 1 人日 | resume 恢复配料；catalog 可用；key 槽迁移幂等 |
| P3 | 前端 4–5 文件，约 1–2 人日 | P3-⑫ 手动清单全过 |
| 收尾 | 文档（README/设置页文案）、CHANGELOG、版本号 | — |

## 8. 附录：涉及文件清单

后端（QAQ-Harness）：

- `crates/qaqh-types/src/session.rs`（+SessionModelOverride、SessionMeta 字段）
- `crates/qaqh-session/src/manager.rs`（persist/clear_model_override）
- `crates/qaqh-domain/src/command.rs`（两个 ControlCommand 变体）
- `crates/qaqh-runtime/src/service.rs`（session.set_model / clear / model_catalog）
- `crates/qaqh-runtime/src/agent/loop_dispatch_control.rs`（两分支）
- `crates/qaqh-runtime/src/agent/state/agent.rs`（apply_session_model / clear_session_model）
- `crates/qaqh-runtime/src/agent/engine_session.rs`（apply_config 合并守卫 + 守卫测试）
- `crates/qaqh-runtime/src/agent/state/lifecycle.rs`（resume/create 应用 override）
- `crates/qaqh-config/src/secrets.rs`（SecretSlot::Provider）、`crates/qaqh-config/src/registry.rs`（merged 只读暴露）
- 测试：`crates/qaqh-runtime/tests/`（新集成用例）

前端（qaqh-winui-app）：

- `apps/winui/src/bridge/types.rs`、`bridge/core_sessions.rs`、`bridge/mod.rs`
- `apps/winui/src/composer_bar/view.rs`（模型 chip）、`composer_bar/mod.rs`（/model）
- `apps/winui/src/settings_view/sections/basic.rs`（文案）
- `apps/winui/src/shell_store.rs`（meta 投影带 model_override）
