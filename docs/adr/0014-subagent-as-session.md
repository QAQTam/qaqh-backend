# ADR-0014 子代理=会话：单权威收敛与跨会话消息

状态: `已采纳（未实施）` · 决策日期 2026-11-12 · 决策人 用户提议 + QAQ agent（宪法第 3 条定稿）

> 引用本 ADR 时注意状态：**已采纳（未实施）≠ 已完成**。落地清单见文末。
> 事实基线：`514082bb`（main）。工作区另有一套未提交的 widget-directive 在途改动
> （102 文件，含 `ConversationSendMessage.widget_context` 契约增量），与本决策正交，
> 但覆盖同一批文件——实施排序见 §后果 第 4 条。

## 背景（已核对事实）

1. **loop 早已共享，不存在"子代理专属重 loop"**：`spawn_subagent_inprocess`
   （`crates/qaqh-runtime/src/registry/spawn.rs:423`）与 `spawn_session_inprocess`
   （同文件 :468）调用同一个 `assembly::assemble`，产出同一个
   `qaqh_agent_runtime::session_loop::SessionLoop`（1942 行，全仓唯一）。
   差异仅 sandbox 标志与工具白名单。
2. **存储层已经是"子代理=会话"**：子代理落 SQLite 会话行，血缘即
   `parent_session_id`（`registry/spawn.rs:268-269` `ensure_session`）；
   `SubagentView` 从工具 effects 投影（`crates/qaqh-session/src/db/view.rs:136`）。
3. **重的是平行官僚机构**（同一事物的第二权威）：
   - AgentPath 树 + `agent_catalog`（`registry/spawn.rs:199-213, 387-400`）；
   - residency / spawn reservation / `spawn_configs`（:360-462）；
   - reload 特权规则"父不加载子不能复活"（:512-547）；
   - collector 线程：300ms 轮询 + deadline + 5+3 次注入重试
     （`crates/qaqh-subagent/src/lib.rs:1237-1519`）；
   - `subagent_outcomes` 独立结果面（registry）。
4. **功能当前悬置**：生产注册器不含任何 subagent 工具
   （`crates/qaqh-workspace/src/registration.rs`；装配点
   `crates/qaqh-agent-runtime/src/session_agent/assembly.rs:275` 传 `&[]`）；
   `SubagentHost::start_subagent` 无生产调用方——canonical edge 消费者
   （旧 `tool_runtime.rs::apply_subagent_spawn_effects`，见 `30f33f1b` #367）
   已随 `34e971cb`「埋葬timeline,移除庞大agentloop状态机」删除。
   wire 命令 `subagent.spawn` 仍 dispatch 到 legacy registry 路径
   （`crates/qaqh-runtime/src/service.rs:463` → `service/handlers/session.rs:277`）。
5. **冻结面约束**：`subagent.spawn`（`crates/qaqh-ringing/src/command.rs:174`）、
   `InterAgentEnvelope`、`Subagent*`/`Team*` DTO 均在 `docs/api-surface/v3.0`。
   ADR-0010 状态「提议中」，§待决 2 **已决：暂不切线** → 不允许 refreeze，
   wire 形状必须逐字节不变；本决策全部落在内核（宪法第 21 条允许）。
6. **新 loop 已具备持久 mailbox**：活动回合时输入入队
   （`session_loop.rs:402-404`），回合边界合流；steer 通道已在
   `ActiveTurn.steers`。结果回传不需要进程内轮询线程。

## 决策

1. `subagent.spawn` 的实现收敛到**唯一的会话创建路径**：会话行 +
   `parent_session_id` 血缘 + 工具白名单 + sandbox 标志。不保留第二 actor 种类
   （`AgentKind::Subagent` 退役）。冻结命令名保留，它是"模型可控的新对话"的
   正式入口，不是兼容别名。
2. AgentPath **降格为会话血缘的投影**：冻结的 `Team*`/`Subagent*` DTO 由
   会话行 + effects 投影供数；`agent_catalog` 不再是权威源。
3. **collector 线程与 `subagent_outcomes` 面退役**：子会话 turn 终态时由 loop
   直接向父会话发 canonical `ConversationSendMessage`（携带既有
   `subagent_terminal` 字段；delivery=Queue，父 idle 时 TriggerTurn），经
   owner/ledger 持久化。无重试线程——mailbox 持久，注入拒绝不再是丢失路径。
4. 超时 deadline 落子会话行；daemon 既有周期任务扫过期并发
   `ConversationCancel`。保留 T-1-2 不变量：被取消的终态不注入正文，
   仅状态面可查。
5. **跨会话消息**：`InterAgentEnvelope.recipient`（本就是 string）允许携带
   session id 跨 root 投递；wire 形状不变，语义扩展以本 ADR 为据。
   保留配额（`admit_outbound_message`）与自送禁止；root 保护改由
   driver/lease 状态表达（用户正在驱动的会话只 Queue 不 Trigger），
   不再用路径特权规则。
6. 子会话 reload 走普通 `get_or_spawn`；废除"父必须加载"特权规则与
   `spawn_configs` 复录机制。
7. 模型工具面按会话语义重写为最小集（spawn / send / wait / list / cancel）；
   模型面非冻结，可自由 redesign。task/board 工具继续悬置；冻结的 Team HTTP
   路由仍由现有存储供数，不在本轮 scope。

## 被否选项

- **保留双机构、只重接 collector 消费者**：延续双权威与 per-child 线程模型。
  P0-① 超时失效 bug（120s 配置实跑 847s）正是该线程模型的产物；mailbox
  已持久，线程是冗余。懒政（宪法第 9 条）。
- **切 LTS-v3.1 线、从 wire 删除 `subagent.spawn`**：ADR-0010 已决暂不切线；
  内核收敛即可达成目标，无需 refreeze。
- **写 subagent→session 转换层声称兼容**：宪法第 9 条明文禁止。正确做法是
  同一实现、冻结入口名，不是两套机构之间加翻译。
- **保留 per-child collector 线程只换 loop**：loop 本就共享（背景 1），
  换无可换；重的是机构不是 loop。

## 后果

1. 冻结 DTO 的**数据源**改为投影，导出物必须逐字节不变：
   `check --against docs/api-surface/v3.0` 必须绿。若投影改写引起字节漂移，
   按宪法第 15 条分类处置，不得手改金标准（第 16 条）。
2. `list_agents`/`wait_agent` 语义从"agent 状态面"变为"会话状态投影"；
   前端 Team 页数据形状不变。旧 collector 测试面随实现退役或重写。
3. 失败模式：跨会话消息风暴 → 既有配额阀；终态消息丢失 → ledger 持久 +
   `SubagentView` 兜底可查。跨 root 投递扩大了攻击面，配额与 driver/lease
   保护是必须项而非可选。
4. **并发排序（宪法第 19 条）**：工作区存在未提交的 widget-directive 在途
   改动（102 文件，含契约增量与金标准重生成），覆盖 `session_loop.rs` /
   `host_impl.rs` / `subagent/lib.rs` / `command.rs` 等本决策实施文件。
   两线正交但同文件：实施前必须由所有者拍板排序（先提交在途改动，或
   明确允许叠在其上），撞车留痕于本 ADR，不得悄悄 rebase 掉。
   **2026-11-12 所有者拍板**：在途改动为 v3.1 genui 线，授权提交 →
   `2ef60928`（feat(genui): 生成式 UI 基础设施，65 文件）。金标准 rebaseline
   差额（contract_hash `bfacb44e…`）**不随该提交入库**，留工作区待 v3.1
   切线时机器重切（规则 16）。子代理收敛在干净代码基线上恢复实施。
   **预存闸门红（与本决策无关，实施时不得顺手"修绿"）**：HEAD 的 v3.0
   金标准树 `e626a3c5` ≠ 登记锁 `e0fc731c`——LTS doc-only-refresh
   （`5d2cdfcd`/`ad32ecad`）重生成金标准后经 `db41f0ea` 并入 main，锁未同步
   重登记。main 上该步 report-only（`surface-policy.yml:79`）。修复路径：
   核对 LTS 线金标准身份后 `--pin-frozen` 补齐，或 v3.1 切线统一裁决；
   禁止在 main 就地重切/偷换锁让它变绿（ADR-0011）。

## 落地清单（未实施，逐条可勾）

- [ ] 切片 A：spawn 收敛——`subagent.spawn` 直走会话创建路径；砍
      `agent_catalog` 平行权威 / residency / reload 特权 / `spawn_configs` /
      `AgentKind::Subagent`（`registry/spawn.rs`、`handlers/session.rs`）
- [ ] 切片 B：回传退役 collector——turn 终态 → canonical 跨会话消息；
      deadline 落行 + 周期扫描（`subagent/lib.rs`、`session_loop.rs`）
- [ ] 切片 C：跨会话寻址——recipient 允许 session id；配额/自送禁止保留；
      root 保护改 driver/lease 表达（`host_impl.rs`、`registry/queries.rs`）
- [ ] 工具面最小集重注册（`qaqh-subagent`、`registration.rs`）
- [ ] 定向验证：`cargo test -p <涉及 crate>`；契约闸门
      `check --against docs/api-surface/v3.0` 绿（wire 不变）
- [x] 并发排序拍板结果回填 §后果 4（2026-11-12：genui 先行提交 `2ef60928`）
