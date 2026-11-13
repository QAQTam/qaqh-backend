# ADR-0015 Goal 模式：持久会话目标 + 空闲自动续跑 + 预算熔断

状态: `已采纳`（切片 A–E 代码落工作区，待提交） · 决策日期 2026-11-12 · 实施 2026-11-13 · 决策人 用户（§已决 四问拍板 + 开工口令）+ QAQ agent（起草/实施）

> 参考实现：OpenAI Codex `codex-rs/ext/goal`（`codex_goal_extension`）。本 ADR 不是移植
> 它的扩展机构，而是把它的**语义**（持久目标、续跑、预算、防失控）落到本仓 ADR-0014
> 已经收敛出的单权威架构上。引用本 ADR 时注意状态：**已采纳（未实施）≠ 已完成**。
> **实施前置**：工作区 widget-directive 在途改动属另一会话，goal 不得触碰同文件；
> 待在途改动提交、基线干净后方可按落地清单实施（所有者 2026-11-12 拍板）。

## 背景（已核对事实）

### Codex goal 模式的工作机制（调研结论，源码在 D:\Projects\codex）

1. 每线程一个持久化 goal（SQLite `thread_goals`）：objective / status / token_budget /
   tokens_used / time_used_seconds；状态机 6 态
   `Active / Paused / Blocked / UsageLimited / BudgetLimited / Complete`。
2. 模型工具三件套 `create_goal / update_goal / get_goal`；`update_goal` 只许
   complete / blocked / paused，resume 与预算态由用户/系统控制。
3. 续跑引擎 = 生命周期钩子 `on_thread_idle`：goal 为 Active 时渲染 continuation 提示词
   （objective + 用量/预算/剩余），以内部上下文 user fragment 调 `start_turn_if_idle`
   自动开新回合（`turn_trigger: "goal"`）。提示词核心是完成审计（逐条需求找权威证据）
   与阻塞审计（同一阻塞连续 ≥3 个 goal turn 才许 blocked）。
4. 记账：每次工具完成/回合结束把 token 与墙钟增量落库；`tokens_used >= token_budget`
   时 DB 层自动转 BudgetLimited，并向运行中回合注入收尾 steering。
5. 防失控三道闸：回合错误（重试耗尽）→ Blocked；连续 3 个自动续跑轮空回复 → Blocked；
   连续 3 轮 exec 失败且无成功工具 → Blocked。
6. 用户入口：TUI `/goal`（set/edit/pause/resume/clear）+ app-server RPC
   `thread/goal/set|get|clear`；运行中改 objective 经 steer 通道注入。

### 本仓已有的全部挂点（ADR-0014 遗产，逐条实测）

1. **输入即续跑通道**：`SessionLoop::send_input`（`crates/qaqh-agent-runtime/src/session_loop.rs:287`）
   ——空闲开回合、忙则入队回合边界合流、`InputPurpose::Steer|Interject` 当轮合并
   （`ActiveTurn.steers`，:139）。`InputPurpose` 四态：TriggerTurn / QueueOnly / Steer /
   Interject（`crates/qaqh-ringing-contract/src/fact/types.rs:870`）。
2. **系统唤醒先例**：`route_exec_exit`（`crates/qaqh-runtime/src/registry/lifecycle.rs:246`）
   已经用 `as_system: true` + `InputPurpose::TriggerTurn` 的 canonical
   `ConversationSendMessage` 唤醒主会话——goal 续跑与它同构，不需要新机构。
3. **终态端口**：`TurnTerminalPort` / `notify_terminal`（`session_loop.rs:169/1746`），
   registry 侧 `route_turn_terminal`（`lifecycle.rs:128`）已按血缘路由子会话终态。
   goal 的记账与续跑判定挂在同一终态事实上。
4. **用量已持久化**：`turns.outcome_json` 存契约 `TurnOutcome`
   （`crates/qaqh-session/src/db/schema.rs:45`、`db/store.rs:612`），
   `Finished { terminal, usage, error }` 携带 `UsageInfo`——goal 记账读终态即可，
   不需要新遥测通道。
5. **模型工具注入**：`SpawnInput.tool_registrars`
   （`crates/qaqh-agent-runtime/src/session_agent/assembly.rs:208`），先例
   `qaqh_subagent::register`（`crates/qaqh-runtime/src/registry/spawn.rs:385`）。
6. **周期任务**：daemon 已有 idle-unload 与子代理超时扫描
   （`crates/qaqh-daemon/src/server.rs:282/302` → `service.rs:280/301`）。
7. **命令分派**：session 目标命令走 `registry/messaging.rs::send_ringing`；daemon
   `Route::Engine` 兜底（`axum_impl/command.rs:360`）。新增命令**零新增路由**
   （规则 16，`conversation.truncate` 先例）。
8. **config 先例**：`SubagentConfig { timeout_secs }`（`crates/qaqh-config/src/config.rs:17`），
   单一真相源 = config，不落 per-spawn 列。
9. **schema 迁移机制**：`SCHEMA_VERSION = 8`（`schema.rs:16`），已有 7→8 一次性迁移先例。

### 冻结面与并发约束

1. ADR-0010 §待决 2 **已决：暂不切线**；`docs/api-surface/v3.0` 一字节不动。
   新增命令/事件/DTO = **wire 实变**，按 `conversation.truncate` 先例记入 ADR-0010
   攒着的 refreeze 清单，main 上 `check --against` 的红是设计状态（report-only）。
2. `COMMAND_CONTRACT_REVISION = "ringing-commands-8"`、
   `SESSION_CONTRACT_REVISION = "ringing-session-2"`（`qaqh-ringing/src/v2/types.rs:76/81`，
   本机实测）。`CommandRejection` 现有 TurnActive / TurnNotFound（:44）。
3. **并发写入者（规则 19）**：工作区有未提交的 widget-directive 在途改动，覆盖
   `qaqh-runtime/src/service.rs`、`service/handlers/session.rs`、
   `registry/lifecycle.rs` 等本决策实施文件（git status 实测）。排序已由所有者拍板
   （§已决 1：在途改动属另一会话，goal 不触碰；ADR-0014 §后果 4 同款先例）。

## 决策（定稿；拍板记录见 §已决）

1. **状态权威 = 会话库**。新表 `session_goals`（schema 8→9，纯增量迁移），每会话至多
   一个 goal：`goal_id / session_id / objective / status / token_budget / tokens_used /
   time_used_seconds / created_at_ms / updated_at_ms` + 续跑审计计数
   （`consecutive_empty_turns` 等）。状态机与 Codex 同构 6 态。**审计计数落库不落内存**
   ——本仓会话会被 idle 卸载，内存态会丢（这是与 Codex 的有意偏差：它的线程常驻，
   我们的不是）。
2. **wire 面三件套**（与 Codex 同构，前端可直接映射）：
   - `RingingCommand::GoalSet { objective?, status?, token_budget? }`（`goal.set`）
   - `RingingCommand::GoalGet {}`（`goal.get`）
   - `RingingCommand::GoalClear {}`（`goal.clear`）
   - `SessionEvent::GoalUpdated { goal: GoalView }`；`GoalView` DTO 入导出物。
   - 拒绝码扩 `CommandRejection`：`GoalExists`（create 撞未完成 goal）/ `NoGoal`。
   - `goal.set/clear/get` 在 service 层直读直写 DB，**冷会话可用**（不要求 loop 已加载，
     与 truncate 的"先拉起"不同——goal 状态不依赖在飞回合）；set 成 Active 后若会话
     已加载则立即触发续跑，冷会话由 §决策 7 的周期扫描接管。
3. **续跑引擎 = registry 终态路由的扩展**，不新增第二机构：
   `TurnTerminalNotice` 到达时（a）从该回合 `outcome_json` 读 usage 累加进 goal 行；
   （b）预算判定 `tokens_used >= token_budget` → `BudgetLimited`，经 Steer 注入收尾
   提示；（c）终态 `Failed` → `Blocked`（防续跑死循环烧 token，Codex 同款）；
   （d）goal 仍 Active 且终态为 Completed **或 Cancelled**（取消不暂停，§决策 4）
   → 先过空回复审计，再向**本会话**发 canonical 续跑消息
   （`as_system: true`、`InputPurpose::TriggerTurn`、
   `message_id = goal-cont:{goal_id}:{turn_id}` 幂等），正文 = continuation 模板渲染。
   空闲开回合 / 忙则入队 / 冷则先加载——全部复用 `send_input` 既有语义。
4. **防失控三道闸**（计数落 goal 行）：
   - 连续 ≥3 个续跑回合空终答且无工具活动 → `Blocked`；
   - 回合终态 Failed → `Blocked`（立即）；
   - 预算耗尽 → `BudgetLimited`（§决策 3b）。
   **用户取消不改 goal 状态（Codex 原样，所有者拍板）**：Cancelled 终态不暂停 goal，
   空闲仍自动续跑；被取消的回合若是**无活动的自动续跑轮**，计入空回复审计
   （Codex `interrupt_turn` + `empty_response_goal` 同律：`automatic && !has_activity
   && interrupted` 才算一次），有活动的取消重置计数。连续 3 次空取消 → Blocked，
   这是用户反复掐灭空转续跑时的出口。
5. **模型工具面**：新 crate `qaqh-goal` 提供 `create_goal / update_goal / get_goal`，
   经 `tool_registrars` 注入，**仅根会话**（子会话是派生瞬时体，goal 语义挂根，
   与 Codex 对 review 子代理禁用 goals 同律）。`update_goal` 只许
   complete / blocked / paused；complete 的工具结果携带最终用量供模型汇报。
   continuation / 完成审计 / 阻塞审计提示词参考 Codex 模板改写为本仓工具名
   （exec / write_stdin / apply_patch…），模板内嵌 crate，不引入模板引擎依赖。
6. **config**：`[goals]` 段，`GoalsConfig { enabled, max_token_budget }`，
   与 `SubagentConfig` 同纪律（越界即错，不静默 clamp）。`enabled` 推荐默认 true
   （Codex Stable 默认开）；`max_token_budget` 可选，缺省 = 无预算（Codex 同）。
7. **daemon 周期 goal 扫描**：扫 DB 中 Active goal（含冷会话）→ `ensure_loaded` +
   发续跑。这使 goal 跨 idle-unload 与 daemon 重启存活——**强于 Codex**（它只在线程
   活着时续跑）。与 idle-unload 天然互斥：续跑开回合即重置 `last_activity_ms`。
   每轮每 goal 至多一条续跑（message_id 幂等 + 回合互斥兜底）。
8. **descendant 记账本轮不做**：首版只记根会话自身回合的 usage；子会话用量汇总
   留后续切片（本仓子会话终态经 canonical 回传，与 Codex 的进程内 descendant
   聚合机构不同，硬搬会造第二权威）。

## 已决（所有者 2026-11-12 拍板，四问四答）

1. **并发排序**：widget-directive 在途改动是**另一会话的活，不得触碰**；goal 本轮
   只做准备（本 ADR），不写实施代码。实施等在途改动提交、基线干净后开始。
   **更新（2026-11-13，所有者口令「你看看差不多你就开工」）**：实测在途 diff 不含
   任何 goal 代码（widget/skills/permission 残余），另一会话已提交 subagent 切片
   E/F（HEAD `fa013859`），ADR-0014 机构在基线内。所有者授权在当前基线开工：
   goal 只新增自己的代码，不提交/不回滚/不改写在途改动；同文件叠加处用精确
   上下文补丁，撞车留痕于此。
2. **revision 取值**：`COMMAND_CONTRACT_REVISION` **-8 → -9**（新增命令即升代，
   与 truncate 同律），随切片 B 落地；`SESSION_CONTRACT_REVISION` 不动
   （事件纯增量，ADR-0010 先例口径）。bump 的代价（未重编旧客户端 open 即 428 拒）
   与 ADR-0010 §待决 1 的"两把锁"账一并记入其攒着的清单。
3. **用户取消语义**：**Codex 原样**——取消不改 goal 状态，空闲仍自动续跑；
   出口是空回复审计（§决策 4）。"取消即 Paused"提案被否，留痕 §被否选项。
4. **冷会话自动续跑**：**要**——周期扫 DB Active goal 自动拉起（§决策 7），
   goal 跨 idle-unload 与 daemon 重启存活。资源代价（8GB 内存机器）已知悉。

## 被否选项

- **移植 Codex 的 extension-api 钩子机构 / 造平行 goal actor**：本仓已有终态端口 +
  canonical mailbox + 周期扫描，第二套机构 = 懒政（宪法第 9 条，ADR-0014 同款批判）。
- **goal 状态与审计计数放内存**：会话 idle 卸载即丢，重启后 goal 变成僵尸；
  DB 是唯一权威（本仓"状态只经 owner/SQLite"的既有纪律）。
- **新增 HTTP 路由 `/ringing/v2/goals/*`**：规则 16 明文禁止；命令走既有
  `POST /ringing/v2/commands`，两张路由表不动。
- **手改金标准让 check 变绿**：规则 16/17、ADR-0011 封死；wire 实变记账等切线。
- **用户取消即 Paused**：否（所有者 2026-11-12 选择 Codex 原样：取消不改状态，
  空闲仍自动续跑）。防骚扰出口是空回复审计：无活动的自动续跑轮被取消计一次，
  3 连即 Blocked。不得重提"取消即暂停"而不先推翻本条拍板。
- **首版做 descendant token 聚合**：子会话回传机构与 Codex 不同，硬搬造第二权威；
  留后续切片。

## 后果

1. **wire 实变**：命令 +3 变体、事件 +1 变体、DTO +GoalView、拒绝码 +2 ⇒
   `[contract-refreeze]`，记入 ADR-0010 攒着的清单；main 上
   `check --against docs/api-surface/v3.0` 必红（report-only，设计状态，ADR-0010 §后果）。
   金标准留给切线时 exporter + `--rebaseline` 机器重切。
   `COMMAND_CONTRACT_REVISION` 随切片 B bump -8 → -9（§已决 2）：未重编的旧客户端
   open 即被拒，不做半兼容——与 truncate 先例同律；5 个由该常量机器生成的金标准
   产物（constants.ts / ContractConstants.cs / 3 fixtures）值差异一并记账。
2. schema 8→9：新表纯增量，旧库直接升级；读侧 fail-closed 纪律不变。
3. 失败模式：续跑风暴 → 三道闸 + 预算 + 回合互斥 + message_id 幂等；
   goal 扫描反复拉起冷会话 → idle-unload 回收，扫描每轮每 goal 至多一条续跑。
4. 资源：本机 8GB 内存 / 8 核 / D 盘余 ~122GB（开工实测）。验证只跑
   `cargo test -p <涉及 crate>`（规则 10/20），不做全量 release 构建。
5. 并发排序（规则 19，已拍板）：widget-directive 在途改动属另一会话，goal 本轮
   只产出本 ADR，不写实施代码、不触碰在途文件；实施以在途改动提交后的干净基线
   为前置。撞车风险与排序结果留痕于此，不得悄悄 rebase 掉。

## 实施记录（2026-11-13，切片 A–E 代码落地）

落点清单（与决策的偏差如实登记）：

1. **事件形状收口**：`SessionEvent::GoalUpdated { goal: Option<GoalView> }` 单变体
   覆盖更新与清除（`None` = 清除），与 `DriverChanged` 同构——不另设 GoalCleared。
2. **预算熔断在回合边界检测**（终态记账），不做 Codex 的工具完成钩子：v1 不给
   loop 加 tool-finish 端口。回合到边界已收尾,熔断语义由状态事件 + 下一轮提示词
   承载;「运行中注入收尾 steer」留后续切片（需要 tool-finish 端口,是独立机构）。
3. **token 记账取终态 `usage.total_tokens`**（`turns.outcome_json` 持久化权威值,
   即回合最后一次 provider attempt 的用量），不是 Codex 的回合内多轮累计 delta——
   多轮工具回合会少计。要精确需 loop 侧累计端口,留后续切片;预算语义因此偏宽松,
   不会误杀。
4. **自动续跑轮识别**：`message_id` 不落库,改靠输入块正文的
   `<qaqh_goal_continuation goal_id="…">` 标记（`view::turn_input_text` 读首块）。
   空回复审计只对带标记的回合计数,用户回合重置计数（Codex 同律）。
5. **goal.get 走同步 ack.result**（`execute_ringing_command` 路径,与 provider.list
   同路），不走 Engine ack；set/clear 同路同步落 DB + 发事件,冷会话可用。
6. **已知竞态（v1 接受）**：周期扫描与终态钩子可能在极小窗口内各投一条续跑
   （扫描已跳过 busy/pending>0 的会话）。重复续跑无害：回合互斥 + 空回复审计仍生效。
   Codex 用 goal-state permit 消除此窗口;本仓 registry 单锁已覆盖大部分,残余窗口
   不另加锁（宪法 5：不为罕见良性竞态造机构）。
7. **模型工具宿主**：`qaqh-goal` crate + `GoalHost` trait（`install_host` 先例,
   daemon `server.rs` 安装）；`ROOT_TOOL_REGISTRARS` 仅根会话注入。工具结果
   `GoalToolOutput` 与 Codex `GoalToolResponse` 同构（goal/remaining_tokens/
   completion_budget_report）。
8. **配置**：`[goals] enabled / max_token_budget`（缺省 true / 无上限）；
   `GoalsPatch.max_token_budget` 双层 Option（null=清除）是 K3「wire 无 null」
   纪律的有意例外,已注明。

## 切片 F：handoff 工作链（2026-11-13，所有者提出缺口后实施）

**缺口**：本仓没有 compact，只有 handoff（模型写交接文档 → 本会话封箱 →
**客户端** `session.create` 开新会话投文档）。goal 若只挂 session_id，handoff
后即失管；更坏的是续跑引擎会向已封箱的死线继续投递。实测基线：sessions 表
无任何 handoff 标记，`from_session_id` 只在工具输出 JSON 里，客户端不回传。

**决策**：goal 挂「工作链」而非单会话。

1. **血缘**：`session.create` 增可选 `handoff_from`（additive wire），客户端
   fork 时传工具输出的 `from_session_id`；`sessions.handoff_from_session_id`
   与建行同事务写入。**内核不猜血缘**（宪法 5）：客户端不传则链断，goal 留在
   旧会话——前端姊妹仓必须配合，契约已备好。
2. **封箱**：`sessions.handoff_sealed_at_ms` 由内核在 `run_handoff` 成功时刻
   写入（owner 泵唯一写口），不靠 tool_calls 文本匹配。
3. **goal 行留在链根，不迁移**：用量跨链天然累计。读侧 `resolve_goal` 沿
   `handoff_chain_root` 上溯；写侧（记账/审计/状态迁移/清除）一律按
   `goal.session_id`（根）定位。环 = 数据损坏，1024 跳 fail-closed。
4. **续跑目标 = 链头**（`handoff_chain_head` 下溯最新后继）：链头封箱 → 跳过，
   周期扫描每轮再探，直到客户端建后继；链头冷 → 先加载。终态钩子与扫描共用
   `deliver_continuation`。
5. **事件**：GoalUpdated 同时发触发会话与链根（不同则双发），两条线的订阅者
   都看得到状态；bootstrap 在链上任何会话都解析出同一个 goal。
6. schema 9 合并两列（未发布，不另开版本）；`NewSession` 结构体不动，
   `set_handoff_from` 独立写口避免 12+ 字面量点连带改。

**已知限制**：handoff 封箱到客户端建后继之间，goal 处于「等待后继」态（扫描
日志可见）；客户端永不 fork 则 goal 停在该态——这是客户端驱动 fork 架构的
固有边界，内核不代建会话（driver 席位在客户端）。

## 落地清单（切片 A–F 代码已落工作区，待提交；定向验证见各项 evidence）

- [x] 切片 B 起同步 bump `COMMAND_CONTRACT_REVISION` -8 → -9，并把本笔 wire 实变
      记入 ADR-0010 攒着的 refreeze 清单（含 5 个常量产物的值差异）。
      evidence: `qaqh-ringing/src/v2/types.rs` 常量 + ADR-0010 清单首条。
- [x] 切片 A：状态面——schema 9 `session_goals` + store CRUD + `GoalView` DTO +
      `GoalsConfig`；迁移与 CRUD 单测。
      evidence: `qaqh-session/src/db/{schema,goals,store,view}.rs`、
      `qaqh-ringing-contract/src/state.rs`、config 四件；
      `cargo test -p qaqh-session` 全绿（含 goals 5 测 + 迁移测试）。
- [x] 切片 B：命令面——`goal.set/get/clear` + `GoalUpdated` 事件 + 拒绝码 +
      service/registry 分派；ADR-0010 清单记账。
      evidence: `qaqh-ringing/src/command.rs`、`handlers/goal.rs`、daemon
      同步命令列表 + 拒绝码 HTTP 映射；`cargo test -p qaqh-ringing` 全绿
      （含 goal 命令 round-trip + 拒绝码稳定性）。
- [x] 切片 C：续跑引擎——终态记账 + 预算熔断 + 三道闸 + continuation 投递。
      evidence: `qaqh-runtime/src/registry/goal.rs` + `route_turn_terminal`
      前置调用；`cargo test -p qaqh-runtime` 全绿。
- [x] 切片 D：模型工具面——`qaqh-goal` register + 内嵌提示词模板 + assembly 注入
      （仅根会话）。
      evidence: `crates/qaqh-goal`、`templates/goal_continuation.md`、
      `ROOT_TOOL_REGISTRARS`、`goal_host.rs`、daemon install_host；
      `cargo test -p qaqh-goal` 2 测全绿。
- [x] 切片 E：daemon 周期 goal 扫描 + 定向验证。
      evidence: `server.rs` 周期任务 + `service.sweep_active_goals`；
      `cargo test -p qaqh-session -p qaqh-ringing -p qaqh-config -p qaqh-config-api
      -p qaqh-goal -p qaqh-runtime -p qaqh-agent-runtime -p qaqh-daemon` 全绿
      （2026-11-13 本机实测，8GB 内存机器，未做全量 release 构建）。
      契约 check（`cargo run -p qaqh-contract-export -- check --against
      docs/api-surface/v3.0`）实测红：`artifact file set differs: 608 vs 600
      files; only-in-first=[cs/GoalStatus.cs, cs/GoalView.cs, cs/GoalsDto.cs,
      cs/GoalsPatch.cs, types/qaqh/{GoalStatus,GoalView,GoalsDto,GoalsPatch}.ts]`
      ——8 个新产物即本笔 wire 增量；值差异（revision -9 常量产物、
      SessionState/SessionEvent/RingingCommand/ConfigDto 形状）在文件集闸之后，
      切线时由 exporter + `--rebaseline` 机器重切。main 上此红是设计状态
      （ADR-0010 §后果），不修绿、不动金标准（规则 16/17）。
      死锁核对：`notify_terminal` 在调端口前已释放 loop 锁；续跑经
      `send_input` 入队、由 `finish_active` 续接，无自锁。`interrupt()` 仅
      Crash 路径（映射 Failed→Blocked 正确）；用户取消走 Cancelled→续跑。
- [x] 切片 F：handoff 工作链——`session.create.handoff_from` + sessions 两列 +
      封箱写口 + 链解析（resolve_goal/chain_root/chain_head）+ 引擎/工具面/
      bootstrap 全走链。
      evidence: `store.rs` 链 helpers + `goals.rs::resolve_goal` +
      `owner::mark_handoff_sealed` + `session_loop::run_handoff` 调用 +
      `registry/goal.rs::deliver_continuation`；链解析单测
      `goal_resolves_along_handoff_chain`（两级链 + 封箱 + 记账定位）。
      定向验证（2026-11-13，-j 2 防 8GB OOM）：session 26 绿（含修复的两个
      迁移夹具：V7 补 sessions 表、混合历史测试改为真 v8 表上重放 8→9）；
      ringing 16 / goal 2 / runtime 54+47+… / daemon 全绿；agent-runtime
      36 绿（--test-threads=1；并行时 `context_overflow_error_lowers_the_denominator`
      因 handoff 全局账本跨测试污染偶发红——基线既有测试基建问题，单跑/单线程必绿，
      非切片 F 引入）。