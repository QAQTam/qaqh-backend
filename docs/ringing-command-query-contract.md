# Ringing v1 命令/查询契约（本轮改造）

本文档是本轮“命令接管 + 持久化”三线并行改造的共享契约。实现时以本文件为准；
后端与前端各自按此对接，冲突时先改契约再改实现。

## 1. 只读查询（GET，Bearer 鉴权）

基础路径：`/ringing/v1/query/...`。全部只读，响应为中立 JSON（snake_case）。

| 端点 | 响应 | 语义来源 |
|---|---|---|
| `GET /ringing/v1/query/session/list` | `SessionMeta[]`（legacy `session.list` 同形状） | `DeepxService::handle("session.list")` |
| `GET /ringing/v1/query/session/meta?seed=<seed>` | `SessionMeta` 或 `null` | `SessionManager::global().load_meta`（service 新增 `session.meta`） |
| `GET /ringing/v1/query/session/activity` | `SessionActivity[]`（legacy `session.activity` 同形状） | `DeepxService::handle("session.activity")` |

未知路径 → `404` + JSON 错误。缺少 `seed` → `400`。

## 2. 命令（POST，Bearer + lease + 幂等）

沿用现有 `POST /ringing/v1/commands/{channel}` 与 `RingingCommandEnvelope`。

### SessionClose 真实语义（本轮核心）

- `ControlCommand::SessionClose { seed }` 不再转发 worker（worker 侧无 Ui2Agent
  等价语义，且 registry.close 会直接杀 worker 进程）。
- daemon 侧拦截：lease/幂等校验通过后，关闭该会话（registry close），经 hub
  `publish_with_causation` 发布 `SessionStateChanged { state: "closed" }`，
  返回 `Accepted`。会话不存在也返回 `Accepted`（幂等关闭）。
- `wire.rs` 的映射错误保留为防御分支（该命令不应再到达 worker）。

## 3. 前端 IPC（Electron main → renderer）

| IPC | 参数 | 行为 |
|---|---|---|
| `ringing:command` | `(seed, channel, envelope)` | `ensureRingingConnected` 后 `POST /ringing/v1/commands/{channel}`，返回 ack |
| `ringing:query` | `(path, params?)` | `ensureRingingConnected` 后 `GET /ringing/v1/query/{path}`，返回 JSON |

renderer 开关：`localStorage["ringing.commands"] === "1"` 时，`session.send_message` /
`session.cancel` / `session.compact` 走 Ringing HTTP；任何失败自动回退 legacy
`request()`（记录日志，不阻塞 UI）。

实现状态（2026-08-01）：
- `ringing:command` / `ringing:query` IPC 已落地（main/preload/electron.d.ts）；
- renderer 共享 helper `src/runtime/ringingCommands.ts`：开关关闭或失败时回退 legacy；
- 带 `files` 的 `session.send_message` 保持 legacy（文件预览展开在 daemon 侧读文件，
  renderer 沙箱内无法复刻，属已知边界）；
- 失败回退 legacy 时，若 Ringing 侧实际已 accepted（网络歧义），可能重复执行；
  命令端点幂等键在 Ringing 内部，legacy 重试无法复用——opt-in 调试开关的已知权衡。

本轮**不切**：`session.new`（Ringing ack 不返回新 seed，需事件驱动创建，属下一轮）、
`session.resume/delete`、`interaction.*`、`skills.*`。

## 4. 持久化 journal（daemon 重启不丢可靠事件）

- 目录：`ensure_data_root()/ringing/journal/{channel}/{seed}.jsonl`（append-only），
  cutover 模式存 `ensure_data_root()/ringing/cutover.json`。
- 启动装载：重建 `ReliableJournal` 与 `SnapshotProjector`；旧 epoch 条目保留
  （event_id 含旧 epoch，新事件不冲突；客户端 epoch 不匹配时按 0 全量回放）。
- I/O 失败只记录日志，绝不阻塞 publish。
- `RingingHub::new(epoch)` 保持非持久默认（既有测试不变）；新增持久构造供
  `server.rs` 使用。

实现状态（2026-08-01）：
- `JournalStore` 已实现（JSONL append + cutover.json 原子写 + 启动装载重建
  journal/router/projection/sequencer/cutover）；`RingingHub::with_persistence`
  已接线到 `server.rs`；runtime 持久化测试通过。
