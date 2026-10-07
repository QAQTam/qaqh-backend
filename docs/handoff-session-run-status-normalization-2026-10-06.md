# Handoff:session.list 归一化裁决 1+2(未收口,勿直接合并)

- 分支:`fix/hotfix`(基于 main `a125400`),工作树 `E:\qaqh-backend-fix`
- 状态:**改动完成、部分测试通过,尚未 commit**;`qaqh-runtime` 测试未跑完(见 §5)
- 日期:2026-10-06
- 缔造者:ZCode session(裁决由用户亲自下达,词表设计与投影优先级经用户确认方向)

## 0. 一段话版本

按用户 2026-10-06 裁决,把 `session.list`/`session.meta` 的运行语义从「worker 进程存在性」(`running: bool`)换成「agentloop 状态」(新枚举 `SessionRunStatus` 九态,权威来源 = canonical fact 投影),并废除 `last_summary` 字段、标题 fallback 截断改为 14 字符。代码改动 7 个文件全部落盘,qaqh-types/qaqh-client/qaqh-title 测试全绿;qaqh-runtime 259 过 1 挂(挂的那个与本次改动无关的 prompt 预算断言,未及验证是否预存在,验证动作被用户终止);qaqh-session 测试因 runtime 失败未单独跑完。**五端前端全部未迁移**,TUI 会编译失败(见 §6)。

## 1. 背景裁决(用户原话要义)

1. **裁决 1**:移除「worker 进程存在」语义,统一用 agentloop 状态标记 session:`idle / working / waiting(等待用户/等待授权) / canceled / error`;状态**优先 control(投影)语义**。
2. **裁决 2**:标题/摘要来源收敛为两个:① LLM 自动生成(既有管道);② 用户首条消息截 14 字符(fallback)。删除「最后一条 assistant 首行截 80 字符」(`last_summary`)。
3. 裁决 3(qaqh-sdk 化)与鸿蒙吃 ts-rs 转译方案:**缓议**,本次不动。

## 2. 调查结论沉淀(为什么这么改——知识资产,勿丢)

### 2.1 改动前的三套并存词表

| 词表 | 位置 | 语义 | 出口 |
|---|---|---|---|
| `qaqh_domain::ActivityState` 六态 `starting\|idle\|working\|waiting_user\|disconnected\|failed` | `qaqh-domain/src/event.rs:90` | live tracker 内存态 | `session.activity` RPC、daemon `/activity`、v1 `ControlState.activity` |
| `session_fact_v2::types::ActivityState` 三态 `idle\|running\|interrupted` | `qaqh-session/src/session_fact_v2/types.rs:789` | **fact 投影持久态**(TurnStarted→Running / TurnFinished→Idle / TurnInterrupted→Interrupted,`projection/control.rs`) | v2 `ControlDelta::Activity`、bootstrap |
| `SessionListEntry.running: bool` | `qaqh-types/src/session.rs`(旧) | `AgentRegistry.instances.contains_key` = worker 进程 spawn 未 close | `session.list` / `session.meta` |

- 用户「control 是 v1 语义」的怀疑**部分成立**:六态 `ControlState.activity` 是 v1 遗产,但 v2 把它整个塞进了 `RingingV2ControlState { base: ControlState, interactions, driver }`(`qaqh-ringing/src/v2/types.rs:388`),v2 线上两套词表并行;webui 曾因读错词表实时路径从未点亮运行态(`webui/src/session/projection.ts:87` 注释自证)。
- 五端消费乱象(调查全文见对话记录):webui `running` 传入后丢弃;TUI activity 优先 running 兜底;Android `session.running || activity=="running"` OR 合并且跨卡片误用全局 activity;Harmony 用 running 冒充运行中(activity 解析了但零消费);WinUI 两视图两套来源用词撞车。

### 2.2 本次的设计决策(逐条含理由)

- **统一词表落在新枚举 `qaqh_types::SessionRunStatus`**(九态,§3),不改造两个旧 ActivityState——旧词表照旧服务各自出口,收敛发生在 session.list 边界。
- **权威来源 = fact 投影,理由**:从持久 canonical log 重放,daemon 重启可重建;live tracker 纯内存,重启即失。投影不可读时才退 tracker。
- **`not_running` 显式存在且不合成**:未加载 ≠ idle。这是 Harmony 把未加载渲染成空闲这一 bug 的结构性解法。
- **只对已加载会话调投影**:`V2ProjectionHub` 首访一个会话会从磁盘全量重放 fact log,`session.list` 对未加载会话逐条调 = 全库重放,性能地雷。已加载判定沿用 `registry.is_running`(此处它退化成「是否值得读投影」的开关,不再上 wire)。
- **waiting 三细分**来自 control 投影 `interactions` 的 `InteractionKind::{Permission,Ask,Plan}`(`session_fact_v2/types.rs:826`),取最后一条未 resolve 未 expire 的;waiting 盖过 working(交互发生在回合中途)。
- **canceled/error 拆分**来自 conversation 投影保留的逐回合 outcome:`Finished{terminal}` Completed/Failed/Cancelled + `Interrupted{reason}`(crash/restart/unknown_fact 归 error,cancel_before_seal 理论上也该归 canceled——**当前实现把所有 Interrupted 归了 error,见 §6 遗留 3**)。
- **`#[serde(other)]` 不加**:未知状态词表取值必须响亮失败而非静默降级(对齐 `InteractionKind` 的既有决策,`qaqh-domain/src/state.rs:90` 注释)。

## 3. 新 wire 契约(G2 v2)

```text
SessionRunStatus(serde snake_case,无未知兜底臂,Default = not_running):
  not_running | idle | working | waiting_permission | waiting_ask | waiting_plan
  | canceled | error

SessionListEntry(SessionMeta flatten +):
  status: SessionRunStatus   ← 取代 running: bool
  workspace_id: Option<String>
SessionMeta 删字段: last_summary(String)
```

`session.activity` RPC、daemon `/activity`、live tracker 六态词表**全部未动**,保留为 fallback 路径与既有消费面。

## 4. 逐文件改动清单(7 文件,全部在 `E:\qaqh-backend-fix`)

1. **`crates/qaqh-types/src/session.rs`**:删 `SessionMeta::last_summary`;新增 `SessionRunStatus`;`SessionListEntry.running` → `status`;G2 闸测试更新(`session_list_entry_wire_keys_are_locked` 键表 last_summary→status、running→status);新增 `session_run_status_wire_vocabulary_is_locked` 词表闸;lib.rs re-export 补 `SessionRunStatus`。
2. **`crates/qaqh-runtime/src/ringing/v2.rs`**:`V2ProjectionHub::projected_run_status(session_dir, session_id)` 新方法——优先级:挂起 interaction(kind 细分 waiting_*)> control activity==Running→working> 最近回合终态(Finished{cancelled}→canceled / Failed→error / Interrupted→error)> idle。imports 补 `ConversationTurnOutcome`、`FactActivityState`、`InteractionKind`、`TurnTerminal`。
3. **`crates/qaqh-runtime/src/service.rs`**:私有 `session_run_status(loaded, fallback, session_id)`——未加载→not_running;已加载→`v2_hub.projected_run_status`(session_dir = `qaqh_types::platform::sessions_dir().join(session_id)`),Err 时 log warn 并退 tracker 映射(Working/Starting→working,WaitingUser→**working(降级,投影不可读无法细分)**,Failed→error,Disconnected→not_running,其余→idle);`list_sessions` 与 `session.meta` 两处接线(fallback 取 `registry.activities()` 里该 session 的状态)。
4. **`crates/qaqh-session/src/manager.rs`**:删 `extract_summary` 及 `save_full_with_watermark`/`save_append` 两处写入点。
5. **`crates/qaqh-title/src/lib.rs`**:`FALLBACK_MAX_CHARS` 20→14;`truncate_title` 截断后补 trim_end(14 字符落在词间空格时不得尾空格);测试期望值同步。
6. **`crates/qaqh-client/src/projection.rs`**(webui/Tauri sanitize 白名单):`running`→`status`,缺失兜底 `json!("not_running")`;测试同步,并把 running 列入必剥离键。
7. **`crates/qaqh-types/src/lib.rs`**:re-export。

## 5. 测试状态(用户已叫停测试,以下为已发生的部分)

| crate | 结果 |
|---|---|
| qaqh-types(含 G2 两闸 + 词表闸) | ✅ 55 passed |
| qaqh-client(白名单) | ✅ 4 passed |
| qaqh-title(14 字符) | ✅ 2 passed |
| qaqh-session(仅 lib 编译)+ qaqh-runtime | ⚠️ 259 passed,**1 FAILED**:`agent::prompt::tests::prompt_and_tool_defs_char_budget`——`identity prompt too long: 10093`(断言 ≤128) |
| 集成测试(qaqh-runtime tests/、daemon e2e、ts-export) | ❌ 未跑 |

关于那个 prompt 失败:本次改动**完全没碰** prompt.rs / identity prompt;10093 字符的 identity prompt 疑似环境注入(skills/workspace?)或 main 预存在问题。用户终止了 `git stash` 对照验证,此判定**未完成**——接手者第一件事:`git stash && cargo test -p qaqh-runtime --lib agent::prompt::tests::prompt_and_tool_defs_char_budget && git stash pop` 确认是否预存在。

## 6. 剩余挂点(按优先级)

1. **补跑测试**:`cargo test -p qaqh-session -p qaqh-runtime`(先裁决 §5 的 prompt 失败归属);然后 daemon/集成;然后 **`just ts-export`**——`SessionRunStatus` 已挂 `#[cfg_attr(feature="ts", derive(TS))]`,webui/src/api/qaqh/ 需要生成 SessionRunStatus.ts 并提交。
2. **五端迁移(各自仓库 PR,响亮失败清单)**:
   - **TUI(E:\qaqh-tui-app)**:类型化解析 `SessionListEntry`——`.running` 字段消失必编译失败;`sidebar_rows` 过滤条件 `entry.running || …`、glyph 兜底 `(None, true) → 暗色○` 全部要改吃 `status`。
   - **webui(E:\qaqh-backend\webui)**:`store.ts:121` `running: item.running === true` → status;`boot()` 里 `find(!archived && running)`;`applySessionMeta` 签名;TabBar 可直接吃 status(替代 activity 合成路径)。
   - **Android(RingingModels.kt:283)**:`last_summary` 字段删除、`running` 删除,DTO 加 `status`;HomeScreen `running || activity=="running"` OR 合并改 status;顺手修全局 activity 套所有卡片的 bug。
   - **Harmony(SessionList.ets)**:同上;列表「运行中」改吃 status,**废除 `running ? '运行中' : '空闲'` 的未加载=空闲合成**——这正是本次裁决要消灭的语义。
   - **WinUI(Wire.cs:252)**:`Running`→`Status` 枚举;侧栏绿点/蓝点改 status;ActivityText 与列表语义统一。
3. **投影归类的已知粗糙点**:`Interrupted{reason: cancel_before_seal}` 当前归 `error`,按裁决语义应归 `canceled`(用户取消);crash/restart/unknown_fact 归 error 正确。一行 match 的事,接手者可顺手改:`v2.rs` `projected_run_status` 里 Interrupted 分支读 reason 细分。
4. **session.activity RPC 与六态 tracker 的退役计划**:本次保留为 fallback;SDK 归一后(v2 订阅流上线)建议整条退役,出口收敛到投影。
5. **v2-smoke.sh / daemon e2e**:`scripts/v2-smoke.sh` 若断言 `running` 键需同步(未检查,主工作树有未提交改动,以 fix 工作树为准检查)。
6. **commit 策略**:验证全绿后建议单 commit:`refactor(session): 统一运行状态词表(G2 v2)——running bool 退役,session.list 内嵌 SessionRunStatus;删 last_summary,标题 fallback 14 字符`。

## 7. 环境备注

- 主工作树 `E:\qaqh-backend`(research/tool-system-modernization)有大量**未提交改动**,与本次无关,勿混;本 handoff 与全部改动都在 `E:\qaqh-backend-fix`。
- 用户偏好:先写 handoff 再说;测试可叫停;裁决由用户亲定,不要替用户重开已决事项。
- 曾有一次 `git stash` 未及 pop 被终止,已恢复核对:7 文件改动完整在盘。
