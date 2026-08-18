# LOOP 重写计划（刀 7 第二阶段 + 刀 10 前置盘点）

> **状态**：设计稿（2026-08-18）。基于第一轮大改（刀 1 全量 + 刀 7 阶段 1）后的现状盘点，
> 为 msgloop 重写提供：职责去留清单、目标模块形态、分阶段执行路径与验收标准。
> 关联：`REFACTOR-ROADMAP.md` 刀 7（统一注入插槽 + loop 架构规范）、刀 10（大文件拆分）；
> `bugfix-1.1-nextdev.md` BUG-009/010。

---

## 0. 重写目标（一句话）

**loop 只留 5 件事：数据流主干、turn 状态机、InjectionBus、admit 调度、事件发射；
身份业务（goal/skills/plan/todo/子代理）与基础设施（上下文组装/token 校准/compact 策略/
标题/持久化）全部移出 loop，各归其位、独立可测。**

对应路线图刀 7 基准的 5 项职责边界（`REFACTOR-ROADMAP.md` §4）：

1. 数据流主干：收 / 反序列化 / 组装 / 发送（gate→tools→repeat）
2. 状态与上下文：turn 状态机、消息历史、compact 时机
3. InjectionBus：subagent / skills / MCP / 系统 / 用户插队统一注入
4. 权限 admit：工具调用执行前过准入
5. 事件发射：流式 token、timeline、工具生命周期

---

## 1. 当前 msgloop 职责盘点（约 8600 行，14 模块）

### A. 编排核心 —— 重写后保留在 loop（约 4900 行）

| 模块 | 行数 | 职责 | 重写处理 |
|---|---|---|---|
| `loop_core.rs` | 1758 | 主事件循环（run/drain_pending）、`dispatch_ringing_one` 命令路由（Control 14/Conversation 5/Tool 2）、`Outcome` 状态机（8 变体）、panic 恢复、会话切换、compact 后台接线、注入吸收、命令幂等 + OperationCompleted/Failed | **保留**，按职责拆分（见 §3） |
| `engine_turn.rs` | 2447 | 数据流主干 `run_lap`（1173-2310，1137 行）：gate→解析→admit→工具→回填→再 gate；stream-block 状态机；ask/plan/权限中断恢复（5 个 handle_*）；abort；auto-compact 决策 | **保留主干**，拆分子模块（见 §3） |
| `engine_tool.rs` | 993 | admit_batch（准入批处理）、execute_and_emit（工具 worker + ActorToolScope）、permission response、progress/denied 事件 | **保留**，admit 审查状态机独立成模块 |
| `types.rs` | 428 | Outcome / RingContext / CancelToken / PendingState / TurnState / Emitter 契约 | **保留**，删除死字段（见 §4） |
| `paced_emitter.rs` | 120 | 事件发射（causation 作用域、writer_dead 守卫） | **保留**，删死字段 |

### B. 不该在 loop 里 —— 重写时移出（约 2000 行）

| 事项 | 当前位置 | 行数 | 移出目标 | 依赖方向 |
|---|---|---|---|---|
| goal-mode 业务 | `engine_input.rs:39-76`（`[QAQ-Harness Goal: resume]` 解析 + todo load/save + Goal→Manual 切换） | ~40 | 独立 `goal_runner`（读 todo 存储，产出注入文本） | goal_runner → qaqh-workspace(todo)；loop → goal_runner |
| skills 业务 | `handle_user_input` 的 activate/build_status；loop_core SkillsActivate/Reload 分支；`state/skill_context.rs` | ~900 | `skill_manager`（独立 crate 或 state 内模块）；skills 作为 InjectionBus 一个 source | skill_manager → qaqh-workspace(skills 发现)；loop 只调 inject() |
| plan/todo 审查拦截 | `engine_turn` 的 `pending_plans`/`pending_todo_activation` | ~80 | 工具层声明（FoldClass / ReviewClass 声明，随刀 6B ③ 落地） | 工具 handler 自带声明；loop 不再特判工具名 |
| 上下文组装 | `AgentState::build_context`（429-491：环境注解 + skills 注入 + token 校准） | ~60 | `context_builder`（消费消息流 + 注解 + skills 快照） | loop → context_builder；context_builder → message/skills |
| token 校准 | `token_calibration.rs` + `estimate_prepared_request` | ~120 | `token_calibrator` 独立模块 | 纯函数服务，无 loop 依赖 |
| compact 内容策略 | `engine_compact.rs`（prompt 模板 + 总结策略） | ~500 | `compact_policy`（策略）与 loop 的触发时机分离 | loop 只保留"何时触发/何时应用" |
| 标题生成 | `engine_title.rs` | 246 | `title_service`（后台任务，event_tx 直发） | 与主循环正交，event_tx 通道已存在 |
| 会话持久化细节 | `rebind_store`/`maybe_save_session`/lifecycle 的 SessionManager/MessageStore 调用点散布 | ~150 | 刀 2 事件溯源日志（Linux 团队推进）；loop 只发领域事件 | loop → domain event；不直接碰 store |
| undo/dashboard/mode | `engine_misc.rs` | 94 | undo → 消息层；dashboard → 投影服务；mode → 会话属性 | loop 只转发命令 |

### C. 死代码 —— 重写时直接删（约 800 行，已逐一核实）

| 死代码 | 位置 | 证据 |
|---|---|---|
| `engine_goal.rs`（GoalEngine）405 行 | 模块 + `Loop.goal` 字段 | 字段只在构造/重置被触碰，方法零调用；goal mode 实际走 handle_user_input 文本路径 |
| `Loop::new_ipc` + reader/writer 线程 ~120 行 | loop_core:260-363 | 生产全走 `from_channels`（worker 已退役 #19）；仅 5 个测试文件使用 |
| `write_ringing_event_frame`/`write_ringing_timeline_intent_frame` ~100 行 | wire.rs | 只被 new_ipc 的 writer 线程调用 |
| `PacedEmitter.cancelled` 字段 | paced_emitter.rs:22 | 自带 `#[allow(dead_code)]`；构造参数全链路无读取 |
| `PendingState.session/new_session/reload_config` + drain 分支 ~20 行 | types.rs / loop_core | 从未被写入（只有清空和读取） |
| `consume_queued_interrupt → terminal_for_queued_interrupt` 管道 | types.rs / loop_core:1720 | 写后从不读（legacy drain 逻辑已删） |
| `resume()` 的 `build_turns_from_context` 计算 ~10 行 | engine_session.rs:61-62 | 结果只用于 log（SessionRestored 已退役） |
| 单变体枚举 `WorkerCommandFrame` | wire.rs | 只剩 Ringing 一个变体，可退化为直接持有 envelope |

### D. 已是好边界 —— loop 只依赖接口，不动

- `ContextFlow`（qaqh-message）：消息入库单一入口 ✅
- `qaqh-gate`：模型调用 ✅
- `qaqh-workspace`：工具执行（已 in-process + ActorToolScope）✅
- `RingingHub` / `AgentRegistry`（qaqh-runtime）：daemon 侧 ✅

---

## 2. 目标形态

```
crates/qaqh-msgloop/src/
├── loop_core.rs          # 主循环 + 命令路由 + Outcome 状态机（~800 行）
├── engine_turn.rs        # run_lap 数据流主干（~900 行）
├── turn_lap/             # run_lap 拆分：gate_lap / parse / admit_dispatch / backfill
├── admit.rs              # 权限准入 + ask/plan/todo 审查状态机（从 engine_tool/engine_turn 收敛）
├── injection.rs          # InjectionBus（刀 7 阶段 2：Preempt/Interrupt 语义）
├── types.rs              # 契约（清死字段后 ~350 行）
├── paced_emitter.rs      # 事件发射（~100 行）
└── 移出：
    ├── goal_runner/      # goal 身份（→ 独立模块或 qaqh-workspace）
    ├── skill_manager/    # skills 身份（→ 独立模块或 qaqh-workspace）
    ├── context_builder/  # 上下文组装
    ├── token_calibrator/ # token 校准
    ├── compact_policy/   # compact 策略（触发时机留在 loop）
    └── title_service/    # 标题生成
```

估算：loop 核心从 **8600 行 → 2500-3000 行**，每个移出模块独立可测。

---

## 3. 分阶段执行路径

| 阶段 | 内容 | 依赖 | 风险 |
|---|---|---|---|
| **S0（前置）** | 合并 PR #30（刀7 阶段1）、#32（改名）；删除 §C 死代码（第一刀清理） | 无 | 零风险，跑测试即证 |
| **S1** | 死代码清理 + 死字段删除 + `new_ipc` 测试迁移（5 个测试文件改 from_channels）后删 new_ipc 链 | S0 | 低 |
| **S2** | `run_lap` 拆分（1137 行 → turn_lap/ 子模块），不改行为 | S1 | 中（纯重构，靠现有测试兜底） |
| **S3** | admit 审查状态机收敛（engine_tool + engine_turn 的 ask/plan 特判 → admit.rs） | S2 | 中 |
| **S4** | 身份业务移出：goal_runner / skill_manager（skills 接 InjectionBus source） | S2 | 中高（skills 行为面广） |
| **S5** | context_builder / token_calibrator / compact_policy / title_service 移出 | S2 | 低-中 |
| **S6** | 刀 7 阶段 2：`Preempt` + `Interrupt/Inline` 调度语义 + 用户插队/skills 迁入 inject() + MCP 判据 | S4 | 中（时机语义一次定对） |
| **S7** | 与刀 2（事件溯源，Linux 团队）对接：loop 的持久化调用点改发领域事件 | 刀 2 | 高（需两团队契约先行） |

每阶段独立 PR、独立回滚点；未达判据不进下一阶段（路线图 §6.5）。

---

## 4. 验收标准

1. loop 只剩 §2 目标形态的文件集合，无身份业务特判（grep 无 goal/skills/plan/todo 关键字分支）
2. 所有非用户消息注入只调 `loop.inject()`（刀 7 判据）
3. MCP / 新身份接入无需改 loop（刀 7 判据）
4. `cargo test -p qaqh-msgloop` 全绿；移出的每个模块有独立单测
5. loop_core / engine_turn 各自 ≤ ~1000 行
6. 无 `#[allow(dead_code)]`、无单变体枚举残留
7. 行为回归：`qaqh-runtime` 集成测试（subagent_inprocess / session_inprocess / host_direct）+ msglp 全量测试通过

---

## 5. 已知技术债（重写时顺带处理）

- `loop_core.rs` / `lib.rs` 顶部架构图仍写 "process-level Loop / SessionBundle" —— 文档脱节，随 S0 更新（2026-08-19 已更新为"单会话 worker 循环"）
- compact 完成路径的注入派发未带 causation 作用域（PR #30 已知缺口）—— 2026-08-19 已修（`inject()` idle 路径 + `dispatch_injections_after_compact` 均进入 causation 作用域）
- BUG-015 单元级回归测试已随 PR #25（`7fc2e40`）同提交
  （`checkpoints_keep_round_text_but_replace_each_stream_block_locally`）；
  run_lap 级"text↔tool 交替单块"断言留待 S2 run_lap 拆分时补
