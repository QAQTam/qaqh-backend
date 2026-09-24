# Ringing v2 单流硬切 Handoff（2026-09-24）

状态：**已落地**。执行 issue **#337**（简化三通道协议）的**方案 B（硬切）**。

## 1. 问题

基线 spec（tag `tui-ringing-v2-frozen-2026-09-23`）§0.2 裁决「canonical fact log 是
唯一权威源；三频道是 **wire 过滤视图**」，但实现把它做成了 3 条**物理**通道：

| 证据 | 现状 |
|---|---|
| daemon `v2.rs` | `events/{channel}` ×3、`commands/{channel}` ×3 |
| runtime `v2.rs` | `subscribe(…, channel, …)` + `event_channel()` 过滤 + 每 channel 各自 replay |
| bootstrap | `tool` 快照是 `control.tools` 的复制（`tool_revision = control.revision`） |
| `qaqh-client/src/v2.rs` | `subscribe_v2(seed, channel, …)` 一次只订一个 channel |

后果：同一 seed 要 3 条 SSE、3 个 cursor、3 次 reset；且客户端必须**跨流归并**
才能满足基线 §11 的 `(fact_seq, projection_index)` 严格有序不变量。

## 2. 修法（硬切）

### 2.1 端点

- `GET /ringing/v2/sessions/{seed}/events/{channel}` → **`GET …/events`**
- 旧形态**删除**，返回 `404`；不提供兼容过滤视图（方案 B）。
- 事件仍带 `stream_key`，客户端 demux。

### 2.2 capability

`open` 响应新增 `capabilities.single_stream`（`#[serde(default)]` → 旧 client 读到
`false`）。新 client 必须显式断言 `true`；为 `false` 时不得回退（旧端点已不存在）。

### 2.3 明确不变

- **命令面保持 per-channel**（`/commands/{channel}`）：channel 在命令面是**路由键**
  （control / conversation / tool 进不同 actor 队列），不是过滤视图。
- **bootstrap 不变**：仍是一份响应里的 `control` / `conversation` / `tool` 三个
  typed state 对象。
- **cursor / reset / replay / interaction / driver 语义全部继承**基线。

## 3. Spec 变更

新增 `docs/spec/2026-09-24-TUI-Ringing-v2单流冻结修订-spec.md`
（tag `tui-ringing-v2-frozen-2026-09-24-single-stream`）。

基线文件按「不得在本 tag 指向的语义上原地改写」保持原样；修订文件只声明差异 +
兼容矩阵（v1 / v2.0 / v2.1）+ TUI 迁移说明。

**wire version 保持 2**：v1→v2 是 cursor 模型代际切换，本次是 v2 内部的传输形态
修订，冻结 tag 是修订标识。

## 4. 改动清单

| 文件 | 改动 |
|---|---|
| `crates/qaqh-ringing/src/v2/types.rs` | `RingingV2Capabilities.single_stream`；新增 `events_path(seed)` |
| `crates/qaqh-ringing/src/v2/mod.rs` | 导出 `events_path` |
| `crates/qaqh-runtime/src/ringing/v2.rs` | `subscribe` 去掉 channel 参数；删 `event_channel` 过滤；`next()` 不再过滤 |
| `crates/qaqh-daemon/.../v2.rs` | `handle_events_v2` 收 `Path(seed)`；open 置 `single_stream: true` |
| `crates/qaqh-daemon/.../mod.rs` | 路由改 `/ringing/v2/sessions/{seed}/events` |
| `crates/qaqh-client/src/v2.rs` | `subscribe_v2(seed, since_cursor)` |
| `crates/qaqh-runtime/tests/v2_acceptance_matrix.rs` | 新增 `single_stream_carries_all_channels_in_global_order`（V2-S1/S2） |
| `scripts/v2-smoke.sh` | 单流订阅 + `single_stream` capability 断言 + `events/{channel}` 404 断言 |

## 5. 验证证据

```text
cargo test --workspace -- --test-threads=1              PASS（138 suites，0 failed）
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo fmt --all -- --check                               PASS
scripts/v2-smoke.sh                                      PASS ×2
```

新增用例：

```text
qaqh-ringing  v2::types::events_path_is_per_seed_single_stream
qaqh-ringing  v2::types::single_stream_capability_is_additive_and_defaults_false
qaqh-runtime  v2_acceptance_matrix::single_stream_carries_all_channels_in_global_order
```

真实 HTTP/SSE（smoke 新增阶段）：

```text
== open advertises single_stream ==
== SSE subscribe (single stream) ==
== per-channel endpoint is hard-cut ==
```

## 6. TUI 侧影响（必须转达）

1. `qaqh-client::subscribe_v2(seed, channel, since_cursor)` →
   **`subscribe_v2(seed, since_cursor)`**（结构体/签名破坏性变更）。
2. **只开一条订阅**；删除「三条流 + 跨流归并」。
3. 按 `event.stream_key` 分发到各 reducer。
4. `open` 后断言 `capabilities.single_stream == true`。
5. reset 只处理一次（单流），不再跨流去重。
6. bootstrap 形态不变。

**硬切语义**：v2.0 client 连 v2.1 daemon 会在 `events/{channel}` 拿到 404。
必须与后端同版本发布。

## 7. 仍未完成（alpha 迭代清单）

1. **P6 设计输入 A（前缀 Segment 分区）**：`system_messages` 仍是插入顺序 + 永远
   前置；中途 `push_system` 会顶到最前，从第一个字节起 cache 全 miss。
2. **P6 设计输入 B（compact 进 canonical log）**：`compact-context.json` 仍是第二
   真源；`build_context_for_gate` 仍整段 clone（请求峰值 2×）。
3. **v1 `Last-Event-ID` → v2 cursor 映射**（已随 v1 端点硬切作废；见
   `2026-09-24-tool-outcome-p2-v1-cursor-closure-handoff.md` §1）。
4. **V2-C3 replaceable producer**（已补，见
   `2026-09-24-ringing-v2-alpha-closure-handoff.md` §2.7）。
5. **driver 侧剩余**：3s 巡检/重启回收与 workspace service gate 已补；仅剩
   `not_eligible` / 显式移交优先级。
6. **崩溃路径 fence 轮转**：`ToolLedger` 的 `Drop` 只覆盖有序退出。
7. **#323 缺口 1/3**（TUI typed payload 消费面）。

## 8. 接手注意

- **不要再给 SSE 加回 channel 段**。频道是视图；要按频道过滤请在客户端按
  `stream_key` 做。
- `events/{channel}` 的 404 是**契约**，不是「还没实现」；不要补兼容过滤视图。
- 单流下 `ResetRequired` 只发一次；任何「每频道一条流各发一次」的假设都已失效。
- `single_stream` 缺省 `false` 是刻意的（旧 client 必须报协议不匹配，而不是静默
  按单流解析）。
- v1 三频道端点（`/ringing/v1/events/{channel}`）**未动**，仍是 2.0 兼容面。
