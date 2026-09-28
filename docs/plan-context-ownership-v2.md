# V2 上下文所有权重构计划（ContextService 倒置 + Plugin 化）

> 状态：提案。前置事实基于 2026-09-27 代码实证。

## 0. 实证结论（为什么这么切）

1. **无外部消费者**：全仓只有 qaqh-session 注释、qaqh-skills 类型引用触及 agent 内部类型。
   loop 重是内生功能堆积，不是外部耦合 → 重构可以在 runtime 内完成，不波及 daemon API。
2. **插件缝隙已存在**：`qaqh-message::ContextFlow` 六个 source 声明式注册，含
   `LifecyclePolicy{undo, compact}`；`InjectionBus` 管 lap 边界注入。
3. **写路径已就位**：PersistOp 队列 = loop 早已以"store 为权威"的方式落盘。
   缺的只是读路径：`build_context()` 读的是 loop 私有的 `AgentState.msg`。
4. **本质有状态、不可无状态化**：运行中 tool 子进程、挂起审批、压缩线程。
   目标不是无状态 loop，而是"上下文权威外移 + loop 只剩引擎状态"。

## 1. 目标形状

```
现状：  SessionManager(磁盘) ←PersistOp── AgentState.msg(loop 内存权威) ←build_context── 各引擎
目标：  ContextService(权威：内存热缓存+磁盘一体, 住 qaqh-message)
          ↑ snapshot()/write_back()/undo()/compact()      ↑ register(plugin)
        loop(引擎状态: turn/tool/injection)     ContextPlugin(=Flow source + 注入 + hooks)
```

- **qaqh-message::ContextService**（新）：包住 MessageStore + ContextFlow + PersistOp 执行。
  读：`snapshot() -> Vec<Message>`（替代 build_context 的读取半边）。
  写：`write_back(receipt)`；undo/compact 按 per-source LifecyclePolicy 执行。
- **ContextPlugin trait**（新，qaqh-message 定义、runtime 实现接线）：
  `source()`（现有 ContextSource）+ `inject(bus)`（现有 InjectionBus 生产者）+
  `on_lap_boundary()`（压缩时机等）+ `on_tool_finished()`（skill/subagent 效果）。
- **AgentState 瘦身**：只留引擎状态（session meta 句柄、endpoint、ephemeral）；
  msg/config/skills/flow 全部移入 ContextService / config 快照。

## 2. 分阶段

### Phase A — ContextService 诞生（qaqh-message 内，无 runtime 改动）
- A1 合成 Service：MessageStore + ContextFlow + PersistOp 执行器合壳，
  `snapshot()/ingest/write_back()` API。
- A2 SessionManager 降级为 Service 的存储后端（load/save 变 backend trait 实现）。
- A3 undo/compact 变 Service 操作：`undo(turn_id)` 按 source 的 UndoBehavior 执行；
  `compact(summary, kept)` 同理。**行为不变，先换家。**
- 验证：qaqh-message 单元测试 + 现有 persist_effects 契约。

### Phase B — loop 断奶（runtime 内，最大单点）
- B1 `AgentState` 拆域：`msg/skills/flow/pending_meta_ops` → ContextHandle；
  state/agent.rs（65KB）目标减半。endpoint/config 变只读快照。
- B2 `build_context()` 改走 Service.snapshot()；PersistOp drain 收敛进 Service.write_back。
- B3 删重建补丁：`subagent_recovery` / `timeline_rebuild` 的"投影vs真相分叉"修复路径
  应当自然消失（分叉源 = 双份真相；权威唯一后不可分叉）。
- 验证：runtime 现有 20 个集成测试（inprocess_loop 等）绿。

### Phase C — Plugin 契约定型
- C1 定义 `ContextPlugin`（见上），把 skills / subagent / goal / mcp_resources
  四个内置 source 迁为 plugin 实例（它们本来就在 register_all 里）。
- C2 tool 效果外移：`tool_runtime::apply_subagent_spawn_effects` /
  `apply_ordered_skill_effects` 改经 plugin 的 `on_tool_finished`。
- C3 终态：`qaqh-runtime::agent` 剩 loop_core + dispatch + turn_lap + 引擎状态 +
  plugins/（compact 等），新上下文功能一律外挂 crate。

## 3. 顺序与风险

- 顺序强依赖：A → B → C。A 可独立合入（纯 message 侧）；B 的 B1 是最大风险单点；
  C 依赖 B 的 ContextHandle。
- 已知风险：
  - undo 跨引擎事务（turn.reset/tool.reset 时序）在 B1 后变成显式契约，需回归
    permission_lifecycle / plan_review_hook；
  - SessionManager 有大量持久化契约测试（save_full 幂等等），A2 是接口搬家不是重写；
  - restart_prefix_cache 依赖 meta 内前缀缓存字段，A2/B1 时字段映射不能动。
- 每阶段收尾 `cargo check --workspace --all-targets`；测试按既定策略后补，
  但 B3 删除重建模块前必须先跑 recovery 相关测试基线。
