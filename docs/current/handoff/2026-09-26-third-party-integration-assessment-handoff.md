# mutilAI-SDK / wsbox 接入评估 Handoff

> 日期：2026-09-26
> 基线：`11371b1`（`main`）
> 状态：完成 CodeGraph 初始化与耦合研判，建议 `mutilAI-SDK` 优先、`wsbox` 分阶段接入
> 评估对象：
> - `/home/qaqtamsy/项目/mutilAI-SDK`
> - `/home/qaqtamsy/项目/wsbox`

## 1. 结论

**优先接入 `mutilAI-SDK`。**

理由不是它功能更多，而是接入边界更窄：

```text
QAQH runtime
  -> qaqh-gate facade
  -> new bridge
  -> mutil-ai ModelAdapter
```

后端只需要在 `qaqh-gate` 内部增加 bridge，保持
`qaqh-gate::chat_stream` / `StreamEvent` 对 runtime 的契约不变。不要采用 mutil 自带的
`Agent` 循环，否则会把 provider SDK 扩散进 agent loop、tool execution 和 canonical
persistence。

`wsbox` 的价值更高但耦合更深：

```text
qaqh-workspace exec/edit/apply_patch/file_mutate
  -> wsbox Session
  -> overlayfs
  -> change set
  -> review
  -> apply/restore
```

它不仅是 sandbox 替换，还涉及写入 ownership、journal、apply/restore、权限确认和
跨平台 fallback。适合第二阶段，先做 shadow / exec-only PoC，不建议首轮全量替换
`qaqh-sandbox + qaqh-workspace journal`。

## 2. CodeGraph 基线

本轮执行：

```text
codegraph init -y /home/qaqtamsy/项目/mutilAI-SDK
codegraph init -y /home/qaqtamsy/项目/wsbox
codegraph sync /home/qaqtamsy/项目/qaqh-backend
```

结果：

| 项目 | 文件 | 节点 | 边 | 语言 |
|---|---:|---:|---:|---|
| `qaqh-backend` | 456 | 12,579 | 50,722 | Rust 为主，含 TS/TSX/YAML 等 |
| `mutilAI-SDK` | 47 | 1,546 | 5,869 | Rust |
| `wsbox` | 24 | 820 | 2,889 | Rust + YAML |

三个索引均为 CodeGraph 1.6.0、extraction version 25、`complete`，无 pending refs。

## 3. 项目定位对比

| 维度 | `mutilAI-SDK` | `wsbox` |
|---|---|---|
| 核心定位 | provider-neutral LLM SDK / protocol adapter | Linux workspace ledger sandbox |
| 主要能力 | normalize、endpoint/profile、stream、retry、cancel、usage、tool result | overlayfs、CAS、ledger、diff、apply/restore、review |
| 与后端重叠 | 与 `qaqh-gate` adapter 重叠 | 与 `qaqh-sandbox` + `qaqh-workspace journal` 重叠 |
| 建议接入层 | `qaqh-gate` 内部 bridge | `qaqh-workspace` exec/mutation 全链路 |
| 跨平台 | Rust/reqwest，跨平台 | Linux 专属，依赖 `nix/libc/overlayfs` |
| 首轮风险 | parity / stream / error taxonomy | 写入 ownership、平台 fallback、daemon 集成 |
| 推荐顺序 | **1** | **2** |

## 4. mutilAI-SDK 耦合面

### 4.1 CodeGraph 关系

关键符号：

- `ModelAdapter`：`src/adapter/mod.rs:43`
- `ChatRequest`：`src/types.rs:746`
- `ChatResponse`：`src/types.rs:903`
- `StreamEvent`：`src/stream.rs:61`
- `EndpointSpec`：`src/profile.rs:77`
- `ProviderProfile`：`src/profile.rs:207`
- `ModelProfile`：`src/profile.rs:640`
- `RequestOptions`：`src/headers.rs:102`
- `ErrorKind`：`src/error.rs:12`

CodeGraph `impact ModelAdapter --depth 3`：

```text
130 nodes / 267 edges
```

影响集中在：

- `src/adapter/*`
- `src/agent.rs`
- `src/blocking.rs`
- `src/lib.rs`
- streaming / retry / cancellation / endpoint tests

这说明 mutil 自己内部有完整 agent-loop 能力，但后端不需要接管这部分。

### 4.2 后端现有 gate 边界

CodeGraph 关系：

```text
qaqh-runtime/src/agent/turn_lap/gate.rs
  -> qaqh_gate::chat_stream(...)
  -> qaqh_gate::StreamEvent::{ContentDelta, ReasoningDelta, ToolCallProgress, ...}

qaqh-runtime/src/agent/engine_compact.rs
  -> qaqh_gate::chat_stream(...)

qaqh-runtime/src/agent/engine_title.rs
  -> qaqh_gate::chat_sync(...)
```

`ProviderConfig` 的 CodeGraph impact：

```text
160 nodes / 238 edges
```

但它仍被限制在 gate facade 内。后端决策 D9 已明确：

- `qaqh-gate` 只负责 provider HTTP/协议/流式事件边界；
- agent loop、tool execution、permission、timeline、canonical persistence 不属于 gate；
- 接入 mutil 必须先经 bridge 和 parity 验收。

### 4.3 建议 bridge 形状

```text
qaqh_types::Message / ToolDef
  -> bridge request mapping
  -> mutil_ai::ChatRequest
  -> ModelAdapter
  -> mutil_ai::StreamEvent
  -> qaqh_gate::StreamEvent
```

必须保持：

- runtime 只看到现有 `qaqh_gate::StreamEvent`；
- cancellation 从 `Arc<AtomicBool>` 映射到 `CancellationToken`；
- retry / reconnect / idle timeout 语义有 parity fixture；
- reasoning、tool-call、server-tool、usage/cache token 不丢字段；
- provider error taxonomy 不靠 message substring 猜测；
- `ProviderConfig` 的现有字段通过 bridge 显式映射，不把 QAQH 语义塞进 mutil。

### 4.4 mutil 的风险

- 0.2 仍在工作区，尚未提交；API freeze 是“候选”，不是稳定发布。
- mutil 自带的 `Agent`、tool loop 与后端 runtime 重叠，必须明确禁用。
- 当前 gate 是 callback streaming，mutil 是 async `ModelStream`，需要一层 stream adapter。
- 后端已有 OpenAI Chat / Responses / Anthropic；mutil 额外支持 Gemini，但需要逐协议 parity。
- 现有后端 tool parser / DSML / XML fallback 不在 mutil 范围内，不能误删。

## 5. wsbox 耦合面

### 5.1 CodeGraph 关系

关键符号：

- `dispatch`：`src/lib.rs:42`
- `Session::open`：`src/session.rs:172`
- `Session::exec`：`src/session.rs:297`
- `Session::apply`：`src/session.rs:1107`
- `Session::restore`：`src/session.rs:1216`
- `ExecParams` / `Change` / `ChangeIndex`：`src/protocol.rs`
- `wsbox-review::Decision` / `route`：`crates/wsbox-review/src/policy.rs`

CodeGraph：

```text
impact Session::apply --depth 3
18 nodes / 18 edges

impact Decision --depth 3
27 nodes / 34 edges
```

`Session::apply` 的连接主要落在 `dispatch`、CLI `run_once` 和 e2e tests；它是
session-owned apply，不是可随手插入的单函数。

### 5.2 后端现有 exec/sandbox 边界

当前：

```text
qaqh-workspace/src/exec/handler.rs::run_exec
  -> direct_exec_sandboxed
  -> qaqh_sandbox::wrap_command
  -> SandboxLaunch
  -> child process
```

CodeGraph：

```text
direct_exec_sandboxed
  1 caller: run_exec

SandboxLaunch
  2 callers: qaqh-sandbox internal + direct_exec_sandboxed
```

单看 exec，wsbox 接入点很小。

但写入链不止 exec：

```text
record_change
  12 callers across
  apply_patch / copy_range / edit / file_mutate / web / journal tests
```

后端现有 journal 是多条 mutation 路径共同调用的旁路记录；wsbox 则是
**session-owned overlay + CAS + change set + apply**。两者不是同一个 ownership 模型。

### 5.3 真正的耦合问题

如果只把 `exec` 换成 wsbox：

- `apply_patch`、`edit`、`file_mutate` 仍直接写真实 workspace；
- wsbox overlay 看不到这些写入；
- change set、diff、restore 不完整；
- `wsbox-review` 无法获得真实全量 change set。

所以完整接入必须至少覆盖：

```text
exec
apply_patch
edit / write / delete
copy_range
web write
permission confirm/apply
journal / code delta
```

这会触及 `qaqh-workspace` 的写入 ownership，而不是单纯替换 sandbox backend。

### 5.4 wsbox 的平台与运行模型风险

- Linux-only：`nix`、`libc`、user namespace、overlayfs。
- macOS/Windows 需要保留现有 `qaqh-sandbox` fallback。
- 当前是 CLI/NDJSON 协议，且 handoff 明确“daemon / streaming stdout 不在本轮”。
- 当前 lock 是 per-session；两个 session 指向同一 workspace 仍可能互相覆盖。
- 崩溃 reconcile 尚未完成：`exec` 中途被杀可能留下未归因 upper 写入。
- `wsbox-review` 仍是 experimental，默认 `RulesOnly`，不应直接启用 AutoApply。

## 6. 决策建议

### 第一阶段：接 mutilAI-SDK

目标：

- 新增 gate bridge，不替换 runtime 契约；
- feature-gated，可一键回退旧 gate；
- 先做 OpenAI Chat / Responses / Anthropic parity；
- 补 cancellation、usage、stream event、error taxonomy、tool result parity fixtures；
- 不引入 mutil `Agent`。

验收门槛：

- 旧 gate 与新 bridge 对同一 mock provider 产生等价事件；
- 取消、retry、reconnect、usage、reasoning、tool call 全部有对照测试；
- canonical/timeline/tool execution 无类型泄漏。

### 第二阶段：wsbox shadow PoC

先不替换写入 ownership：

- 用 wsbox 在独立 workspace 做 exec shadow；
- 对比现有 `direct_exec` 与 wsbox 的 diff/ledger；
- 验证 Linux capability、overlayfs、session lock、crash 行为；
- 将 `wsbox-review` 设为 `RulesOnly + shadow`，只记录不 AutoApply。

### 第三阶段：再决定是否合并写入路径

只有 shadow PoC 证明：

- 全量 mutation 都能进入同一 change set；
- apply/restore 与现有 permission/confirm 语义一致；
- Linux-only fallback 对 Windows/macOS 无破坏；
- 崩溃恢复与并发 session 语义可接受；

才考虑用 wsbox 替换或包裹 `qaqh-sandbox + workspace journal`。

## 7. 最终排序

```text
1. mutilAI-SDK
   - 边界清晰
   - 可 feature-gated
   - 与 qaqh-gate 已有设计决策一致
   - 首轮不触碰 canonical / runtime / workspace

2. wsbox
   - 安全价值更高
   - 但触及 exec + 所有写入路径 + journal + apply/review
   - Linux-only，且仍是 checkpoint/experimental
   - 先 shadow，后 ownership 迁移
```

如果目标是“尽快形成可验证的第三方接入”，先做 `mutilAI-SDK` bridge。

如果目标是“直接提升 agent 写入安全和可回滚性”，应先做 `wsbox` shadow PoC，但不要把它
误判成低耦合的 sandbox 替换。

## 8. 接手注意事项

- 不要把 mutil `Agent` 引进 `qaqh-runtime`；只使用 adapter / normalization / stream。
- 不要让 mutil 类型越过 `qaqh-gate` 公开边界。
- 不要在 parity 测试前删除旧 gate。
- 不要把 wsbox 只接到 `exec` 后宣称“所有写入可回滚”；当前 edit/apply_patch 路径仍会绕过。
- wsbox 必须保留非 Linux fallback，不能把 daemon 启动绑死在 overlayfs。
- `wsbox-review` 默认只能 shadow / rules-only，不能直接 AutoApply。
