# ADR-0033 AgentTeam 2.0：同事=会话，一块任务板一条通道

状态: `已采纳` · 决策日期 2026-11-20 · 决策人 仓库所有者（指示原话："全部落main吧"——
refreeze 落 main 版本位吸收，不取道 LTS-v4.1 分支；`subagent.spawn` wire 变体按草案
倾向删除）· 决策人 QAQ agent 考据定稿

> 一句话：学 dsh（dpsk-harness）的 AgentTeam 演进结论，复用本仓 ADR-0014/0018 已收敛的
> "子代理=会话"内核，把表面重立为——**同事（colleague）不是子代理，是模型拉起的新对话；
> 协调靠一块共享任务板（CAS），通信只走一条通道（canonical mailbox），完成通知免费
> （终态合流已存在）**。本 ADR 同时登记一批 wire 实变，按 ADR-0031 治理走版本位。

## 背景（每条均已核实，附出处）

1. **内核已就绪，无需重建**：spawn 走唯一会话创建路径（`registry/spawn.rs:133`
   `spawn_subagent_v2`，血缘 = `sessions.parent_session_id`，同一条 `assemble` 装配）；
   终态经 `TurnTerminalPort` 自动合流回父会话 mailbox（`registry/lifecycle.rs:230`），
   ADR-0018 修复后终态说真话。**"让模型拉起新对话、新对话干活、干完回传"三件事的机械
   已 100% 存在**。
2. **表面拧巴清单**（2026-11-20 考据）：spawn 输出双 ID 同值异名（`session_id` ==
   `child_session_id`，`host_impl.rs:89-117`）；AgentPath 降格为投影却仍是模型面主寻址
   （回落名 `sub_<id12>` 纯为过 path 校验，曾炸出 ULID 大写缺陷）；`wait_agent` 一工具两
   语义且 mailbox 等待实为 50ms 内存轮询（`host_impl.rs:236-275`）；board_* 十件协作
   工具中留言板五件复制了 send_message+mailbox；Team 投影说谎（Idle/Cancelling →
   Running，`axum_impl/v2.rs:293-298`）+ 三个硬编码死字段；`TeamDelta` 9 种增量事件
   契约定义但全仓零发射方；任务四形复制（DB 行/工具面/wire/工具面快照）。
3. **dsh 同型演进先例**（dpsk-harness 三篇已实施决策笔记 + 升级指南）：
   - `2026-10-15-agent-teams-as-subagents`——一代 Team 平行机构（名册/邮箱/
     `spawn_teammate`/Team 作用域通信工具）整体退役，teammate 就是 subagent；
   - `2026-10-29-inbox-only-agent-wait`——**任务变更不通知、不唤醒、不投递**（试过
     任务变更唤醒等待者后回滚：无关编辑打断所有等待成员）；需要谁行动就发明确消息；
   - `2026-10-29-team-task-only-projection`——投影只留任务+模式，名册/邮箱集合
     无消费者读取、退役校验器还会阻断任务操作，删除；
   - 升级指南 `agent-team-subagents`：全部换标准 subagent 工具 + Session id 寻址。
   dsh 现存 Team 面 = 4 个任务工具（create/list/get/update，CAS by revision，
   write_scopes advisory+重叠警告，ready 由依赖派生，reassign 仅根）+ team:policy 段
   + 根开关（mode 按根保存、后代继承、工具条件注册）。**没有留言板。**
4. **冻结面边界（v4.1，ADR-0032 切线基线 main `bb28211b`）**：
   - **在冻结面（wire 实变）**：`TeamResponse`（含 `board` 字段）、`TeamSnapshot`/
     `TeamAgentSnapshot`/`TeamAgentStatus`/`TeamAgentResidency`/`TeamInboxSummary`/
     `TeamBoard*` 五型、`TaskBoardSnapshot`/`TaskView`、`SessionSettingsPatch`、
     `RingingCommand` 的 `subagent.spawn` 变体、`ToolEffectInfo::SubagentSpawned`、
     `SubagentView`。
   - **不在冻结面（自由改动）**：模型工具面十件的 args/output/描述/policy
     （`SpawnSubagentOutput` 等工具输出类型零导出）；内部 SQLite schema
     （`team_tasks`/board 表）；内核装配、血缘、终态合流。
5. **治理成本已松绑**：ADR-0031 后 refreeze = main 版本位 minor bump + 金标准机器重切
   （参考 `acfe7618` created_at_ms additive 批次），不再攒账等切线；ADR-0032 已开
   LTS-v4.1 线（main ⊇ LTS-v4.1 超集不变量）。
6. **前端影响**：Team 页消费 `/team` 快照（roster+inbox+tasks+board）；姊妹仓需同批
   改（board tab 退役、状态文案、根开关 UI）——前后端共进退纪律（ADR-0018 §后果 2 同律）。

## 决策

1. **同事=会话（承 ADR-0014，词汇收编）**：模型面唯一身份 = **session id（权威）+
   name（spawn 时起、兄弟间唯一、显示与任务 owner 寻址用）**。AgentPath 从模型工具面
   退役；内核投影保留供管理面。工具更名或保留原名均可（模型面非冻结），但 args/output
   词汇统一为 id+name。
2. **通信单通道**：board_* 留言板五工具退役，留言板机构（BoardHost/板表读面）随切片 D
   一起删除，**不留恒空死投影**。协调事件只走 canonical mailbox：
   - 完成通知 = 终态合流（已存在，免费）；
   - 中间进度/请求行动 = `send_message`（明确消息）；
   - 任务状态查询 = 任务板（拉）。
3. **任务板对齐 dsh 语义**（模型面 + 内核，非冻结）：`task_create` 增 `write_scopes`
   （advisory 工作区相对前缀）；`task_list` 增 `ready`/`owner` 过滤 + 游标分页；
   `task_update` 增 `reassign`（仅根）与 `edit`；ready 由未完成依赖派生（纯函数）；
   重叠警告 = 进行中任务 write_scopes 前缀相交时在视图附 `write_scope_warnings`
   （警告不阻止任何操作）。`team_tasks` 加列走内部迁移。**任务变更永不投递/唤醒**
   （policy 段明文，dsh 同款）。
4. **wait 单语义**：`wait_agent` 只保留 targets=终态等待（全部目标 settled 即返回，
   携带各自终态摘要）。mailbox 水位等待模式退役或改真实 canonical watermark
   （实施时定稿），50ms 内存轮询不留。
5. **Team 根开关**：`team/mode` 落根会话设置（`SessionSettingsPatch` additive 字段），
   后代继承存活根的模式；协作工具（task_* 五件）按根模式条件注册
   （`ROOT_TOOL_REGISTRARS` vs `SESSION_TOOL_REGISTRARS` 挂点已在 `spawn.rs:11-22`）。
   默认关（dsh 同律）。关闭时保留任务与同事，只是摘工具与 policy 段。
6. **wire refreeze（一次吸收，per ADR-0031 版本位）**：
   - `TeamResponse` 删 `board` 字段；`TeamBoard*` 五型删除；
   - `TeamAgentSnapshot` 诚实化：status 读真话（Idle≠Running）、name 取 spawn 名
     （fallback id 派生）、`current_task_id` 接真数据或删字段、residency 接真或删；
   - `TaskView` additive：`write_scopes`/`ready`/`write_scope_warnings`；
   - `SessionSettingsPatch` additive：`team_mode`；
   - `RingingCommand.subagent.spawn` 变体**删除**（wire 入口恒 fresh 无 fork，能力
     是工具面真子集；保留只会维持双入口双命名规则——工具面是唯一入口）；
   - `ToolEffectInfo::SubagentSpawned` 三字段冻结形状不动。
7. **policy 段改写**：`[Team policy]` 按 dsh 语义重写（write scopes 记录在任务、
   CAS 认领、任务变更不通知、需要行动发消息、根等 wait 后再答），与协作工具声明
   同进同退（现挂点 `session_agent/prompt.rs:50-71`）。

## 被否选项

- **保留留言板**：dsh 已证伪——同一事件通道的第二机构 + 通知耦合 + 无消费者的
  死投影维护成本；本仓 board 还自带 post→订阅者扇出（`BoardPostOutcome.notified`），
  正是 dsh inbox-only 笔记否掉的"每次提交通知等待者"变体。
- **任务变更自动通知/唤醒**：无关编辑打断所有等待成员；任务日志已留修订，逐成员
  通知凭空造出未读状态与投递恢复（dsh 被否选项原文照录）。
- **自建 Team 成员身份/名册**：第二权威，ADR-0014 已清算过一轮；dsh 同样归一。
- **给"完成回传"建新通道**：终态合流已存在且 ADR-0018 修真；新通道=重复机构。
- **保留 `subagent.spawn` wire 命令**：恒 fresh 无 fork，能力是工具面真子集，
  双入口双命名规则是已实测出缺陷的温床（ULID 大写回落名）。
- **一次性全删（含 wire-clean 部分等 refreeze 一起做）**：切片 A/B 不撞冻结面，
  先行落地可独立验证；refreeze 面集中一次吸收，减少金标准重切次数。

## 后果

1. **契约分类**：删型/删字段 = 实变；additive 字段（team_mode、write_scopes/ready/
   write_scope_warnings）= 实变（additive 放松 bump 纪律见 ADR-0031，仍走版本位 +
   金标准重切）；模型面/内部 schema = 导出物零变化。
2. **前端同批**（姊妹仓）：Team 页 board tab 退役；roster 状态文案换真话值域；
   writeScopes/ready 展示；根开关 UI（模型/推理菜单，dsh 同位）；前后端共进退。
3. **版本位**：按 ADR-0031 落 main 版本位 minor bump + 机器重切；与 LTS-v4.1 线的
   合流顺序由所有者拍板（本 ADR 不锁定）。
4. **行为变化**：模型面 board 工具消失（依赖它的在途回合按工具缺失自然降级到
   send_message/task_*）；Team 页不再显示留言板；空闲子代理显示 Idle 而非 Running。
5. **迁移**：`team_tasks` 加列（NULL 容忍 + 视图派生），board 表保留数据但停读
   （切片 D 删读面；写面随工具退役即时消失）。

## 落地清单（逐条可勾）

- [x] 切片 A（wire-clean）：模型面词汇统一——五件工具 args/output 收编为
      session id + name；spawn 输出去 `timeis`/双 ID 死字段（模型可见 JSON
      恰三键 `{session_id,name,content}`，闸门
      `typed_output_model_and_display_share_the_same_payload`）；内部
      `ToolEffect::SubagentSpawned` 瘦身为与冻结 `ToolEffectInfo` 一致的三字段
      （唯一消费方 `tool_job::effect_infos` 实测编译绿）；`wait_agent` 单语义
      （targets 必选、mailbox 水位模式与 50ms 内存轮询退役、`WaitAgentOutcome`
      死字段 `activity_fact_seq` 删除，闸门
      `wait_agent_schema_requires_targets_and_optional_timeout`）；policy 段
      改写（单通道/session id 寻址/任务变更永不通知）。
      定向验证（`cargo test -j 8`）：tool-core 74 绿 / subagent 11 绿 /
      agent-runtime 53 绿（含 policy 闸门）/ runtime lib 59 绿。
      契约导出物零变化（未触碰任何冻结类型）。
- [ ] 切片 B（wire-clean）：任务板 write_scopes/ready/重叠警告/reassign/游标
      —— 工具面 + `db/team.rs` 迁移 + 派生纯函数 + 闸门测试（先红后绿）。
- [ ] 切片 C+D（refreeze 一次）：Team 根开关（SessionSettingsPatch additive +
      条件注册）+ `TeamResponse.board`/`TeamBoard*` 删除 + `TeamAgentSnapshot`
      诚实化 + `TaskView` additive + `subagent.spawn` 变体删除 + board 机构删除
      + 金标准重切 + 版本位 bump；姊妹仓同批改。
- [ ] 冒烟：`smoke-subagent.ps1` 扩展——lead 建任务（带 write_scopes）→ 子级 CAS
      认领 → 子级终态合流回父（"已完成"）→ lead wait 收答；根开关关态反证
      （工具不在场）。
- [ ] 并发留痕（规则 19）：实施期共享树同文件改动按既有纪律部分暂存。

## 参考

- dsh 决策笔记三篇 + 升级指南（`D:/Projects/dpsk-harness/.agents/notes/implemented/`
  `.../architecture/2026-10-15-agent-teams-as-subagents.zh.md`、
  `.../simplification/2026-10-29-inbox-only-agent-wait.zh.md`、
  `.../simplification/2026-10-29-team-task-only-projection.zh.md`、
  `docs/upgrade-guide/v0.2.1-rc.2/agent-team-subagents/guide.zh.md`）
- dsh Team 工具面：`packages/team/tool-agent-team/src/index.ts`（TEAM_POLICY/四工具/
  根开关 when 条件）；服务域：`packages/team/agent-team/`（README 四承诺）
- 本仓拧巴考据：ADR-0014/0018 + 2026-11-20 表面考据（spawn 双 ID、wait 双语义、
  Team 投影说谎、TeamDelta 零发射、任务四形）
- 冻结面清单：`docs/api-surface/v4.1/types/qaqh/`（Team*/TaskView/
  SessionSettingsPatch/RingingCommand/SubagentView）
