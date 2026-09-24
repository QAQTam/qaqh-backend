# TUI Ringing v2 单流冻结修订（2026-09-24）

状态：**FROZEN**  
适用：TUI beta、Windows alpha、后续桌面壳层  
Wire 标识：`schema = qaqh.Ringing`，`version = 2`（不变）  
本修订 tag：`tui-ringing-v2-frozen-2026-09-24-single-stream`  
基线：`docs/spec/2026-09-23-TUI-Ringing-v2冻结语义-spec.md`
（tag `tui-ringing-v2-frozen-2026-09-23`）

> 本文件是 v2 冻结语义的**修订**，不是新 wire 代际。
> 基线文件按「不得在本 tag 指向的语义上原地改写」的约定保持原样；本文件只声明
> **差异**，其余全部继承基线。
>
> **cursor / reset / replay / interaction / driver 语义全部不变。**

## 0. 修订裁决（delta）

基线 §0.2 裁决：**「三频道是 wire 过滤视图，不是事实源」**。

本修订把这句话执行到底：

| # | 基线 | 本修订 |
|---|---|---|
| 1 | 三频道是 wire 过滤视图 | 三频道仍是**视图**，但由**一条** per-seed 流承载；`stream_key` 是唯一归属判据 |
| 2 | `GET /sessions/{seed}/events/{channel}`，客户端开 3 条 SSE | `GET /sessions/{seed}/events`，客户端开 **1** 条 SSE |
| 3 | 每条流各自 cursor / replay / reset | 单流一个 cursor、一次 replay、**一次** reset |
| 4 | `capabilities` 无单流标记 | 新增 `capabilities.single_stream` |
| 5 | `POST /commands/{channel}` | **不变**（channel 仍是命令路由键，见 §3） |

**理由**：频道既然是过滤视图，就不该要求 3 条物理连接。基线把「视图」实现成了
「传输分区」，导致客户端必须跨流归并才能满足基线 §11 的
`(fact_seq, projection_index)` 严格有序不变量。

## 1. 端点面（delta）

| 用途 | 基线 | 本修订 |
|---|---|---|
| 频道 SSE | `GET /ringing/v2/sessions/{seed}/events/{channel}` | `GET /ringing/v2/sessions/{seed}/events` |
| 其余端点 | 见基线 §1 | **不变** |

规则：

- **硬切**：`events/{channel}` **删除**，不提供兼容过滤视图，返回 `404`。
- 单流上事件的 `stream_key` 取值仍为 `{"kind":"channel","data":"control"|"conversation"|"tool"}`
  或 `{"kind":"resource",…}`（基线 §5 映射表不变）。
- 客户端**必须**按 `stream_key` demux；不得假设「一条流 = 一个频道」。
- `since_cursor` 语义不变：opaque token，服务端签发，客户端原样回传。

## 2. 版本协商（delta）

`POST /ringing/v2/clients/open` 响应新增：

```json
{
  "capabilities": {
    "subscribe": true,
    "interact": true,
    "drive": true,
    "timeline": true,
    "service": true,
    "content": true,
    "single_stream": true
  }
}
```

- `single_stream: true` = 服务端只提供 §1 的单流端点。
- `#[serde(default)]`：旧 client 反序列化新响应时该字段为 `false`。
- **新 client 必须显式断言 `single_stream == true`** 才可假定单流语义；为 `false`
  时不得回退到 `events/{channel}`（那已不存在），应报协议不匹配并提示升级。
- 未知 capability 字段仍按基线忽略。

## 3. 命令面（明确不变）

`POST /ringing/v2/commands/{channel}` **保持 per-channel**。

理由：channel 在命令面是**路由键**（control / conversation / tool 进入不同的
actor 命令队列），不是过滤视图。把它从 body 推断只是把同样的信息换个位置，
不减少状态面，反而让「命令去哪个队列」变成隐式推断。

## 4. 继承项（全部不变）

以下全部按基线执行，本修订不做任何改动：

- §3 Canonical cursor（`(log_id, fact_seq, projection_index)`、token 编码、推进规则）。
- §4 Snapshot 与原子订阅（bootstrap 形态、replay 只回放 reliable、live 缓冲）。
- §6 Delivery 语义（reliable / replaceable / ephemeral）。
- §7 `ResetRequired`（reason 集合与客户端处理）。
- §8 Pending interaction（`interaction_id` 重放、first-answer-wins）。
- §9 driver capability（显式 claim/release、`driver_epoch`、`DriverChanged`）。
- §10 v1 兼容映射（`Last-Event-ID` → v2 cursor，服务端负责）。
- §11 TUI SessionModel 不变量。
- §12 Windows alpha 约束（共用同一语义，不分叉）。

**单流对 §7 的影响**：`ResetRequired` 在单流上**只发一次**（基线是每条流各发一次）。
客户端的 reset 处理逻辑不变，只是不再需要跨 3 条流去重。

**单流对 §11 的影响**：reducer 不再需要跨流归并——单流天然满足
`(fact_seq, projection_index)` 严格递增。

## 5. 兼容矩阵

| 平面 | v1（2.0 兼容面） | v2.0（基线冻结） | v2.1（本修订） |
|---|---|---|---|
| 订阅 | `/ringing/v1/events/{channel}`，`Last-Event-ID` | `/ringing/v2/sessions/{seed}/events/{channel}` × 3 | `/ringing/v2/sessions/{seed}/events` × 1 |
| cursor | `epoch:channel:stream_seq` | canonical `since_cursor` | 同 v2.0 |
| reset | `ringing.reset_required` | `ResetRequired`，每条流一次 | `ResetRequired`，**单流一次** |
| 频道归属 | 端点即频道 | 端点即频道 | **`stream_key`** |
| `capabilities.single_stream` | 无 | `false`（字段缺省） | `true` |
| 命令面 | `/ringing/v1/commands/{channel}` | `/ringing/v2/commands/{channel}` | 同 v2.0 |
| 生命周期 | 2.0 起一发布周期映射层 | 被本修订取代 | 当前权威 |

**硬切影响**：v2.0 client 连 v2.1 daemon 时，`events/{channel}` 返回 `404`。
不提供自动降级——客户端必须与后端同版本发布。

## 6. TUI 迁移说明

1. `qaqh-client::subscribe_v2(seed, channel, since_cursor)` →
   **`subscribe_v2(seed, since_cursor)`**；`RingingChannel` 参数删除。
2. **只开一条订阅**。删除「按 channel 开三条流 + 跨流归并」的逻辑。
3. 事件处理改为按 `event.stream_key` 分发：
   - `Channel(Control)` → control reducer
   - `Channel(Conversation)` → conversation reducer
   - `Channel(Tool)` → tool reducer
   - `Resource { .. }` → resource reducer
4. **bootstrap 不变**：仍是一份响应里的 `control` / `conversation` / `tool`
   三个 typed state 对象。
5. reset 处理：只处理一次（单流），不需要跨流去重。
6. `open` 后断言 `capabilities.single_stream == true`，否则报协议不匹配。
7. 静态门禁不变：TUI 只通过 `qaqh-client` 消费，不直接依赖
   `qaqh-ringing` / `qaqh-domain` / `qaqh-session`。

## 7. 验收

| ID | 场景 | 必须成立 |
|---|---|---|
| V2-S1 | 单流覆盖三通道 | **一条** SSE 上收到 `control` / `conversation` / `tool` 三类 `stream_key` |
| V2-S2 | 单流全局有序 | 单流内 `(fact_seq, projection_index)` 严格递增，无 gap 无 dup |
| V2-S3 | per-channel 硬切 | `events/{channel}` 返回 404，不返回兼容过滤视图 |
| V2-S4 | capability | `open` 返回 `capabilities.single_stream == true` |
| V2-S5 | 单流 reset | 落后 buffer 时单流发**一次** `replay_overflow` |
| V2-C1..C7 / R1..R4 / D1..D3 | 基线矩阵 | 全部在单流上仍然成立 |

实现证据：

- `crates/qaqh-runtime/tests/v2_acceptance_matrix.rs::single_stream_carries_all_channels_in_global_order`（V2-S1/S2）
- `scripts/v2-smoke.sh`（V2-S3/S4 + 真实 HTTP/SSE 单流）
- `crates/qaqh-daemon/src/axum_server.rs::v2_driver_handover_emits_reliable_control_event`（单流上的 reliable replay）
