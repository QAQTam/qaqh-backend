# Workspace 升级：Capability 抽象 + MCP + Hooks 权限模型（计划）

> 状态：待实施（2026-08-14 定稿）
> 范围：`crates/deepx-workspace`（`manager.rs`/`permission.rs`/`authorization.rs`/`runtime.rs`）+ 新 `mcp`/`hooks` 模块
> 关联：[PLAN-SUBAGENT-STATUS-AREA.md](./PLAN-SUBAGENT-STATUS-AREA.md)、[subagent-exec-sandbox.md](./subagent-exec-sandbox.md)

## 1. 结论 / 决策

- 权限管理的对象从"工具"提升为**能力（Capability）**：tools、MCP 工具、hooks 都是能力的载体；`admit(capability, args, 资源意图)` 泛化，不再依赖工具名字。
- **前置已完成（2026-08-14）**：`ToolHandler.category` 声明成为权限分类单一事实源，`categorize_tool` 名字表删除——这是 Capability 抽象的第一步，本节其余为后续升级内容。
- `ToolManager` **保留**：收窄为"静态工具注册表 + 执行协调器"（register/lookup/defs/allowlist/prepare/finalize）；风险与类别元数据随 Capability 定义。
- MCP 动态工具注册时补一个 Capability（类别按命名空间约定，兜底保守），**权限体系第一次覆盖所有载体**。
- Hooks（pre/post 执行链钩子）走**独立于 admit 的半信任模型**：workspace 内部注册，只做路径/上下文隔离，不弹窗。

## 2. 现状（代码实证，2026-08-14）

- ✅ `ToolHandler.category`（`crates/deepx-workspace/src/lib.rs` L532+）：13 工具 + spawn_subagent 全量声明；`web_fetch` 修正为 Net。
- ✅ `ToolInvocation.category`（`authorization.rs` L32+）+ `runtime::lookup_category`（`runtime.rs`）+ 两处构造点（`execution.rs` / `engine_tool.rs` L156/L416）统一 lookup。
- ✅ `categorize_tool` 已删除（含 6 个 git_* 死条目）；`needs_permission` 用声明值，`process` action 特判保留（`permission.rs`）。
- ✅ 回归：workspace 204 + msglp 65 tests 全绿（含 glob→Read 走 needs_permission 断言）。
- 🔲 MCP：无任何 MCP 代码（`Cargo.toml` 无 mcp 相关依赖）。
- 🔲 Hooks：执行链（`execution.rs` execute_authorized：prepare→execute→finalize）无钩子挂点。
- ⚠️ `ToolManager::stats()` 无生产调用者；`deepx-workspace::manager::ToolExecReport` 与 `deepx-message::effect::ToolExecReport` 同名双结构（升级时清理）。

## 3. 决策点

| # | 事项 | 建议 | 截止 |
|---|---|---|---|
| D1 | Capability 结构形态 | `struct Capability { id, name, category: ToolCategory, risk: ToolRisk, resource: ResourceIntent }`；静态工具 = 编译期构造，MCP = 运行时构造 | 实施时 |
| D2 | MCP 默认分类策略 | 命名空间约定优先（`mcp://read/*` → Read）；无约定兜底 **Exec**（保守：动态代码执行面） | MCP 接入时 |
| D3 | hooks 信任模型 | 半信任：workspace 内部注册（仅 daemon/壳侧钩子），**不弹窗**，隔离层 = 路径白名单 + 上下文只读快照 | 实施时 |
| D4 | ResourceIntent 抽象 | `paths / network / env / process / context` 五类；`extract_target_paths` 演进为按 intent 提取 | 实施时 |
| D5 | ToolManager 清理项 | ToolStats 接回 dashboard 或删除；双 ToolExecReport 合并 | 升级轮一并 |

## 4. 目标架构

```text
Workspace（能力注册与执行环境）
├── Capability：id + name + category + risk + ResourceIntent + 执行入口
│     ├── 静态工具（handler fn | HTTP 转发）          ← 今日 ToolHandler 演进
│     ├── MCP 工具（动态发现 → 包装为 Capability）     ← 新增
│     └── Hooks（pre/post 闭包，独立信任模型）        ← 新增
├── admit(capability, args) → AutoApprove / AskUser / Deny（泛化，不碰名字）
├── SafetyPolicy（risk × workspace 边界，路径级阻断）   ← 保留
└── ToolManager（静态注册表 + 执行协调，职责收窄）      ← 保留
```

### 4.1 Capability 定义（草案）

```rust
pub struct Capability {
    pub id: String,          // 全局唯一（静态工具 = tool name；MCP = "mcp:{server}:{tool}"）
    pub category: ToolCategory,
    pub risk: ToolRisk,
    pub intent: ResourceIntent,   // paths/network/env/process/context
    pub execute: CapabilityExecutor,  // Local(fn) | Http | Mcp { server, tool } | Hook { phase }
}
```

`ToolHandler` 保持（模型 schema/defs 生成需要 description+input_schema），内部携带 `capability: Capability`；`to_tool_def` 不变。

### 4.2 MCP 接入

- 新模块 `mcp/`：client 发现工具（`name/description/inputSchema`）→ 按 D2 分类 → 构造 Capability → 注册进 manager（`all_defs` 自动包含 → 模型可见）。
- 执行：`CapabilityExecutor::Mcp` → mcp client 调用；超时/取消沿用 PreparedCall 骨架。
- 权限：admit 路径与静态工具完全一致（category 驱动）；沙箱下 MCP 默认 Denied（B1 语义：动态执行面不进子代理，除非显式白名单）。

### 4.3 Hooks 权限模型

- 挂点：`execute_authorized` 的 prepare 前（pre）/ finalize 后（post）；hooks 注册时声明 `ResourceIntent`（默认 `context:read`）。
- 半信任：hook 由 workspace 内部注册（壳/daemon 配置），**不经过 admit**；隔离 = intent 白名单 + 只读快照（hook 拿到上下文副本，不能改主状态）。
- 审计：hook 执行写入 audit 日志（`audit.rs` 扩展 hook 类型）。

## 5. 实施阶段

| 阶段 | 内容 | 验收 |
|---|---|---|
| P1 ✅（已完成） | handler.category 单一事实源 + 删名字表 | 2026-08-14 提交 `6c7fb4e`；204+65 tests 绿 |
| P2 | Capability 结构落地：ToolHandler 内嵌 capability、ResourceIntent 引入、admit 签名更新 | 行为零回归；既有测试全绿 |
| P3 | MCP client 接入（依赖评估：`rmcp`/`mcp-rs` 或最小自研 JSON-RPC over stdio/SSE） | 动态工具可注册/执行/审批 |
| P4 | Hooks 挂点 + 半信任模型 + 审计 | pre/post 示例钩子（如审计钩子、workspace 切换钩子） |
| P5 | ToolManager 清理：ToolStats 接线或删除、双 ToolExecReport 合并 | clippy 干净、无死代码 |

## 6. 变更记录

| 日期 | 改动摘要 | 触发原因 |
|---|---|---|
| 2026-08-14 | 初稿：Capability 抽象 + MCP + hooks 权限模型；P1（category 单一事实源）已落地 | workspace 升级规划 + glob 权限漏配事故 |
