# 工具运行时上下文：线程局部 → 显式传递（迁移方案）

**状态**：设计稿，**未实施**（2026-10-31）。等 `qaqh-runtime` 三拆的编译验证通过后再动。
**触发**：2026-10-30 设计债审计——「本来可以直连 API，却要先装好线程局部状态」。
**规模**：`crates/qaqh-workspace/src/runtime.rs` 592 行；crate 内读 ambient 的点 32 处、
`with_manager` 调用点 14 处（全仓）。

## 1. 现状：同一份信息有三个载体

| 载体 | 位置 | 写入方 | 读取方 | 问题 |
|---|---|---|---|---|
| `TOOL_MANAGER: OnceLock<Mutex<ToolManager>>` | `runtime.rs:31` | 进程启动（daemon/CLI） | `with_manager` 兜底 | 与下面那条**双源** |
| `ACTOR_TOOL_MANAGER: thread_local<Arc<Mutex<ToolManager>>>` | `runtime.rs:41` | actor 线程 `install_actor_tool_manager` | `with_manager` 优先 | 是否存在取决于线程历史 |
| `RUNTIME_CTX: thread_local<RuntimeContext>` | `runtime.rs:28` | `set_context` / `install_tool_ctx` | `context()` | 隐式主体身份 |
| `AGENT_MODE: thread_local<Cell<u8>>` | `runtime.rs:46` | 同上 | `current_mode()` | 0/1/2 魔法值 |
| 沙箱标志、fold 策略 | `runtime.rs`（thread_local） | 同上 | 工具 handler | 同上 |
| `CANCEL_SESSION_RESOLVER: RwLock<Option<fn() -> Option<String>>>` | `qaqh-permission/src/lib.rs:195` | **runtime 注册**（`runtime.rs:53`） | `is_cancel()` | **反向闭包注入** |
| `ACTOR_SESSION` / `ACTOR_CANCEL` / `SESSION_CANCELS` | `qaqh-permission/src/lib.rs:64/166` | 取消路径 | `is_cancel()` | 取消状态按 session 存，靠 resolver 找 session |
| `ToolCallContext`（**显式**） | `qaqh-tool-core`（`tool_api/context.rs`） | 调用方构造 | 工具 handler | 已经是正确形状 |
| `ToolCtx`（**显式**） | `runtime.rs:205` | serve/CLI/测试 | `execute_with_context` | 与 `ToolCallContext` 字段重叠 |

三条具体病灶：

1. **双源**：`with_manager` 拿到的 manager 取决于「本线程是否装过 actor manager」。
   同一个 handler 在 actor 线程与 daemon 线程看到不同 allowlist。
2. **反向注入**：`qaqh-permission` 的 `is_cancel()` 需要 session 才能查取消表，于是
   runtime 在首次 `set_context` 时注册一个读 `RUNTIME_CTX` 的闭包。注释自己写着
   「打断 lib.rs ↔ runtime 的反向依赖环」——环没断，只是从编译期挪到了运行期。
   若同线程没调用过 `set_context`，取消检查静默失效。
3. **同一次调用两套上下文**：`execute_with_context(ctx)` 先把 `ctx` 装进线程局部，
   再经 `execute_authorized` 从线程局部把同一份信息读回来重建 `ToolCallContext`
   （2026-10-31 已就地修掉这条往返，见 `execution.rs` 的 `Admission::Authorized` 分支）。

## 2. 目标

`ToolCallContext` 是**唯一**载体：入口（agent tool job / daemon / CLI / 测试）构造它，
往下逐层显式传参。线程局部只允许作为「兼容视图」，由 `install_tool_ctx` 在入口处
派生，handler 全部迁移完成后整段删除。

## 3. 迁移步骤（每步独立可验证）

**S1 · 冻结清单**（只读，1 小时）
`rg -n "runtime::context\(\)|current_mode\(\)|current_workspace\(\)|is_cancel\(\)" crates/qaqh-workspace/src`
产出 32 处的表格（文件:行 → 需要的字段），作为 S2 的进度条。

**S2 · handler 改吃显式字段**（最大块）
按 S1 清单把 `crate::runtime::context()` / `current_mode()` 换成 `ToolCallContext` 字段。
`ToolCallContext.workspace_root` 已存在但被 `active_workspace_root()` 绕过
（后者读 `current_workspace()`），这一步顺带修正：显式上下文优先，进程级工作区只作
`None` 时的兜底。

**S3 · 取消显式化**（跨 crate）
`is_cancel()` → `context.cancellation.is_cancelled()`。`qaqh-permission` 去掉
`CANCEL_SESSION_RESOLVER` 与 `SESSION_CANCELS`（改为由调用方持有取消位）。
这是唯一需要改 `qaqh-permission` 公共 API 的一步，爆炸半径：`rg "is_cancel" crates`。

**S4 · 删双源**
daemon 启动时显式 `install_process_tool_manager()`；删 `TOOL_MANAGER` 兜底与
`ACTOR_TOOL_MANAGER`，`with_manager` 变成「拿不到就报错」。
此时 `runtime.rs` 的 5 个 thread_local 全部消失。

## 4. 验收

```bash
# 1. 线程局部/进程级 manager 归零
rg -n "thread_local!|static TOOL_MANAGER" crates/qaqh-workspace/src
# 2. ambient 读取点归零（除 install_tool_ctx 自身）
rg -n "runtime::context\(\)|current_mode\(\)" crates/qaqh-workspace/src
# 3. 行为等价
cargo test -p qaqh-workspace -p qaqh-runtime
cargo test -p qaqh-permission
```

## 5. 风险与不做的事

- **不**把 `ToolManager` 改成 `Arc<ToolManager>` 无锁：工具注册仍需 `&mut`，锁是必要的，
  要删的是「锁在哪找」而不是「有没有锁」。
- **不**动 `ToolCtx` 与 `ToolCallContext` 的合并：两者字段重叠但 `ToolCtx` 是
  宿主侧入口参数、`ToolCallContext` 是执行期契约，合并属于另一项设计。
- S3 之前不要删除 `ACTOR_SESSION`：取消测试（`permission_lifecycle` 等）依赖它。
