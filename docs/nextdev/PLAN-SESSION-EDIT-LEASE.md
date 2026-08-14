# 会话编辑租约（EditLease）传话系统 + Subagent Co-work 路线（计划）

> 状态：待实施（2026-08-14 定稿）
> 范围：`crates/deepx-domain`（ToolFinished 扩展）、`crates/deepx-daemon`/`deepx-runtime`（EditLeaseRegistry + 注入）、`crates/deepx-workspace`（files_affected 上报）、`apps/winui`（徽标/提示）
> 关联：[PLAN-WORKSPACE-CAPABILITY.md](./PLAN-WORKSPACE-CAPABILITY.md)、[PLAN-WINUI-WORKSPACE-NAV.md](./PLAN-WINUI-WORKSPACE-NAV.md)、[dsh-alignment.md](./dsh-alignment.md)、[subagent-exec-sandbox.md](./subagent-exec-sandbox.md)

## 1. 结论 / 决策

- **EditLease（编辑租约）**：daemon 侧单例注册表，记录 session → 修改范围（精确文件 + 所属 crate 根 + workspace 根三层粒度）；写工具成功后经 `ToolFinished.files_affected` 上报；TTL 衰减释放。
- **通知型（不强制锁）**：冲突时双向注入 as_system 系统消息（「⚠️ [SESSION 'B' 正在修改 X]」），模型自行决策避开/等待/合并；不做写拒绝（保自主性）。
- **模型上下文通道 = as_system 注入**（2026-08-14 已落地的通道），前端 UI = sidebar 徽标 + composer 提示（P2）。
- **Subagent co-work v1 可行，形态 = hub-and-spoke**：主代理为协调中枢（派活/收结果/转发），子代理为执行者；子代理间只通过 EditLease 互相感知"谁在改什么"，不直接通信。
- **co-work 硬前置 = busy 注入拒绝修复**（`loop_core.rs` L424-441：回合运行中注入被拒且不重试）——子代理活跃回合中收不到冲突注入，co-work 知情权失效。

## 2. 现状（代码实证，2026-08-14）

- ✅ `files_affected` 已在 worker 侧生成：`extract_files_affected(&name, &audit_args)`（`crates/deepx-workspace/src/manager.rs` L268），写工具执行后进入 `ToolExecReport`——只差经 `ToolFinished` 上报 daemon。
- ✅ daemon → worker 命令通道：`send_ringing_command`（`crates/deepx-runtime/src/service.rs` L54）可向任意 session 发命令；`as_system: true` 注入通道（`ConversationSendMessage` 扩展）2026-08-14 落地。
- ✅ WorkspaceStore（`crates/deepx-session/src/workspace.rs`，300 行）：session→workspace 归属已可查（`{data_dir}/workspaces.json`，cwd 自动 attach）——传话系统的组织底盘。
- ✅ 会话活动状态：`SessionActivityTracker`（`crates/deepx-runtime/src/activity.rs`，per-seed）——lease 衰减的辅助信号。
- ⚠️ `ToolFinished`（`crates/deepx-domain/src/event.rs` L418）无 `files_affected` 字段——需扩展（serde default 兼容）。
- ⚠️ busy 拒绝：主代理/子代理回合运行中注入被 `poll_interrupts` 拒绝（`crates/deepx-msglp/src/ringing_v1/loop_core.rs` L424-441），collector 只 warn 不重试（`crates/deepx-subagent/src/lib.rs` L503）——co-work 前置缺陷。
- ⚠️ 子代理：独立 Ringing 会话（ephemeral），session-scoped todo.json；子代理间零互相感知。

## 3. 决策点

| # | 事项 | 建议 | 截止 |
|---|---|---|---|
| D1 | 锁强度 | **通知型**（注入提示，不拒绝写）；强锁留作二期评估 | 实施时 |
| D2 | lease 粒度 | 文件精确 + crate 根推导（向上找 `Cargo.toml`，非 Cargo 项目用目录深度启发式）；workspace 根作兜底 | 实施时 |
| D3 | TTL | 30min 无写操作衰减释放；回合结束**不释放**（跨回合工作） | 实施时 |
| D4 | 注入去抖 | 同一 session 对同一范围冲突 5min 只注入一次 | 实施时 |
| D5 | 子代理参与 | 子代理**登记** lease（防互相覆盖）但**不接收**低价值广播；冲突注入仅发给活跃子代理 | 实施时 |
| D6 | busy 修复归属 | collector 注入失败重试（指数退避 3 次）/或 daemon 侧排队——**co-work 硬前置** | co-work P1 前 |
| D7 | 共享任务板 | co-work v1 **不建** workspace 级 todo；任务分发走主代理 spawn context 文本 | co-work P1 |
| D8 | co-work 通信形态 | **hub-and-spoke**：子代理↔主代理（现有注入）；子代理↔子代理不做直接消息（会话状态不可控） | 定稿 |

## 4. 实施细节

### 4.1 EditLeaseRegistry（daemon 侧）

```text
{ workspace_id → { session_seed → Vec<LeasePath> } }
LeasePath = 精确文件路径 | crate 根目录 | workspace 根（层级前缀，冲突按前缀判定）
```

- 注册：`ToolFinished { files_affected }` → 匹配 session 的 workspace（WorkspaceStore）→ 三层推导 → upsert lease + 重置 TTL；
- 冲突检测：新注册集合 vs 其他 session 现存 → 文件相等 / 路径前缀重叠；
- 广播：Control 频道新增 `EditLeaseChanged { session, workspace, paths, active }`（前端 P2 消费）；
- 注入：命中冲突 → `send_ringing_command(目标 seed, ConversationSendMessage { as_system: true, text: "⚠️ [SESSION 'X' 正在修改 Y——合并时注意]" })`，去抖（D4）；
- 清理：TTL 扫描（daemon 定时器，30s 粒度）+ session 归档/删除钩子。

### 4.2 协议扩展（两处，serde default 兼容）

1. `ToolFinished`（domain/event.rs L418）加 `#[serde(default)] files_affected: Vec<String>`；
2. Control 频道新增 `EditLeaseChanged` 事件（字段见 4.1）。

### 4.3 前端（P2）

- sidebar 会话行：活动编辑徽标「✎ X」（`shell_store.rs` 投影 `edit_leases`，复用 `session_rev` 刷新）；
- composer 工作状态区：冲突提示条（复用 `work_status_area` 模式，独立一行，可关闭）。

## 5. Subagent Co-work 可行性分析（基于 EditLease）

### 5.1 可行形态：hub-and-spoke（结论：**可实现，v1 边界清晰**）

```
主代理（协调中枢：派活/收结果/转发）
 ├─ spawn A「改 msglp 的 types.rs」 ──→ [A: 写 types.rs]── lease{types.rs}
 ├─ spawn B「改 msglp 的 engine_tool.rs」─→ [B: 写 engine_tool.rs]── lease{engine_tool.rs}
 │     （A/B 若重叠：EditLease 冲突注入 → 后到者自动避开）
 ├─ A 完成 → system 注入（现有机制）→ 主代理评审
 └─ 接力：主代理把 A 结果摘要塞进 B 的 context 再 spawn（现有 context 参数）
```

### 5.2 可行性逐项验证

| 能力 | 现有组件 | 缺口 |
|---|---|---|
| 子代理独立执行 + 结果回传 | ✅ spawn_subagent + system 注入 | 无 |
| 子代理间文件冲突感知 | EditLease（本 PLAN P1） | 无 |
| 注入送达活跃子代理 | `send_ringing_command` 可发任意 session | **busy 拒绝**（D6 硬前置） |
| 任务分发/接力 | spawn `context` 参数 + 主代理上下文 | 无 |
| 共享任务板 | ❌ todo 是 session-scoped | v1 不做（D7） |
| 子代理直接互发消息 | 技术可行（都有 seed）但回合状态不可控 | v1 不做（D8） |

### 5.3 风险

- **注入可靠性**（最大）：不修 busy 拒绝则冲突提示在子代理活跃时丢失 → D6 前置；
- **子代理噪音**：每个子代理都登记 lease 会增加注册表噪声 → D5（子代理只登记、广播限主代理可见 + 冲突注入定向）；
- **主代理带宽**：多子代理结果注入排队 → 现有注入机制已天然排队（回合边界），可接受。

## 6. 实施阶段

| 阶段 | 内容 | 验收 |
|---|---|---|
| P1（本 PLAN） | ToolFinished.files_affected + EditLeaseRegistry + 冲突注入（去抖/TTL） | 双会话同 crate 修改互见提示；workspace/msglp 测试绿 |
| P2 | EditLeaseChanged 广播 + 前端徽标/提示条 | 侧栏可见「✎ X」；composer 冲突条 |
| D6 修复（co-work 前置） | collector 注入重试/daemon 排队 | 子代理活跃回合中注入不丢 |
| Co-work P1 | 主代理并行多子代理 + EditLease 冲突感知（hub-and-spoke） | 两子代理分工不同文件零冲突；同文件后到者感知 |
| Co-work P2 | 主代理接力编排（结果摘要 → context 传递） | 多轮接力完成跨子代理任务 |
| Co-work P3（评估） | workspace 级共享任务板（todo 扩展） | 独立决策 |

## 7. 变更记录

| 日期 | 改动摘要 | 触发原因 |
|---|---|---|
| 2026-08-14 | 初稿：EditLease 传话系统 + Subagent co-work 路线（hub-and-spoke 可行，busy 修复为硬前置） | 多会话二次提交问题 + co-work 前瞻 |
