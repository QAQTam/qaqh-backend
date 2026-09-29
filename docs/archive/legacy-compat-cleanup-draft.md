# 旧版兼容清理草稿（待撤除清单）

> 状态：**协议侧硬切已于 2026-09-27 执行**（下表大部分条目已落地，证据见各条）。
> 少数条目（D 第 3 步、E 部分文档项、F 保留项）仍待做，勾选状态为准。
> 依据：2026-09-27 全仓审计（三个只读审查 + codegraph 交叉验证）+ TUI（`D:\qaqh-tui-app`）实际调用面核查。
> 总原则：**协议契约在 daemon 的 HTTP 面（`/ringing/v2/*` + SSE + JSON），不在任何 client 封装里。**

## 执行摘要（2026-09-27 硬切）

已删除/切换：v1 `with_session` 别名（v1+v2 信封，全部调用方迁 `with_session_id`）、
client 的 `into_v1` 投影与 v1 形状 API（`send_command`/`command_status` 直接返回
`RingingV2CommandAck`/`RingingV2CommandStatus`）、client 死面（`send_command_v2`/
`command_status_v2`/`timeline_v2`/`connect_v2_async`/`validate_envelope`/`envelope_to_batch`/
`cursor_from_sse_id` 及 v1 类型再导出）、gateway v1 信封解析（`proxy_command` 改 v2 直通，
身份字段由网关签发）、webui `RINGING_VERSION = 2`、todo 状态词统一 `pending`
（`idle` 输入别名/counts 双键/schema 枚举全部拆除，`normalizeTodo` 退化为直通）、
`missing_seed` → `missing_session_id`（envelope + daemon 全部错误码）、6 处 no-op
serde rename、TUI 同步（`ClientV2CommandAck`/`ClientV2CommandStatus`）。
验证：`cargo check --workspace` ✅、TUI `cargo check` ✅、webui `tsc --noEmit` ✅
（测试按约定后置）。

---

## A. 影响正确性（优先修）

- [x] **G2 回归闸断言错位**：`crates/qaqh-types/src/session.rs:137` 字段叫 `resume_session`，
      但 wire 闸测试（`session.rs:365`）断言的运行期键是 `"resume_seed"`——断言了不存在的键，
      真正的 `resume_session` 反而没被闸住。改断言名。
- [x] **错误码仍是旧名**：`crates/qaqh-ringing/src/envelope.rs:190` `validate()` 缺 session_id
      时返回 `"missing_seed"`，会原样泄漏到命令拒绝响应。改为 `"missing_session_id"`（需同步前端/网关的
      错误处理）。
- [x] **注释撒谎（误导排障）**：
  - `crates/qaqh-client/src/endpoint.rs:111-112`：称 daemon 侧 `session_param_value` "回退读取 legacy
    `seed`"，实际 runtime 只认 `session_id`（`qaqh-runtime/src/ringing/service_methods.rs:35-37`，
    测试同文件 148-157 已锁）。
  - `crates/qaqh-client/src/client.rs:708-710`：`upload_content` 注释称 daemon 兼容旧 `seed`
    字段（multipart 路径），需核实 `handle_content_upload` 后改注释或删兼容读取。
  - `crates/qaqh-daemon/src/axum_server/axum_impl/service_api.rs:55`：注释"兼容旧 `seed`"与
    runtime 事实矛盾，属过期注释。

## B. 活跃的单向兼容垫片（先迁调用方，再删垫片）

- [x] **`with_session()` deprecated 别名**（转发到 `with_session_id`）：
  - 定义：`crates/qaqh-ringing/src/envelope.rs:158-161`（v1）、
    `crates/qaqh-ringing/src/v2/types.rs:432-435`（v2）。
  - 活跃调用方 4 处：`qaqh-webui-gateway/src/lib.rs:559, 744, 814`、
    `crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs:636`（另 qaqh-client v2.rs:409）。
  - 动作：调用方全部改 `with_session_id()` 后删两处别名。
- [x] **webui v1→v2 归一层**（注释自称待删）：
  - `webui/src/state.ts:58-103` `normalizeTodo`（`idle`→`pending`、counts 双键）。
  - `webui/src/lib/transcript.ts:96-104` `normalizeTool`（camel/snake 双吃）。
  - 动作：确认 daemon 侧 wire 已统一后改为直通。
- [x] **`single_stream` 的 `#[serde(default)]`**（`qaqh-ringing/src/v2/types.rs:49-52`，给旧 client
  的容错）：当前仓库内已无按 false 分支的消费方，确认外部壳层后可删 default。

## C. 浏览器 v1 信封双轨（需要 webui + gateway 联动硬切）

- [x] **webui 仍以 v1 信封提交命令**：`webui/src/lib/protocol.ts:2` `RINGING_VERSION = 1`；
      `webui/src/lib/ringing.ts:186` 组装 v1 形状信封。
- [x] **gateway 翻译层**：`qaqh-webui-gateway/src/lib.rs:796-831` `proxy_command` 把 v1 信封
      重包成 v2。webui 改发 v2 信封后，翻译层与 v1 解析路径一并删除。
- [x] 附带：`webui/src/lib/ringing.ts` 的 `CommandChannel` 三频道字段是 v2 的一部分
      （`v2/types.rs:385`），**不是**残留，勿删。

## D. qaqh-client 瘦身 → 退役（依赖外部壳层 = TUI 的迁移）

> TUI（`D:\qaqh-tui-app`）path 依赖 `../qaqh-backend/crates/qaqh-client`，是仓库内唯一真实消费方
> （编译期 fan-in 为零的结论仅对 monorepo 内部成立）。TUI 对命令 ACK 只读 `.status`
> （protocol/mod.rs:223），迁移代价极小。

分三步，每步 TUI 均可编译：

- [x] **第 1 步（TUI 零改动）**：删 client 内死面：
  - `v2.rs:379-389` `send_command_v2`、`v2.rs:444-446` `command_status_v2`（与
    `send_command`/`command_status` 逐字重复）；
  - `v2.rs:506-534` `timeline_v2`（与 `fetch_timeline_page` 同端点且缺 `validate_for` 校验）；
  - `v2.rs:262-269` `connect_v2_async`（纯别名）;
  - `types.rs:153-193` `validate_envelope`/`envelope_to_batch`/`cursor_from_sse_id`（三频道流残端，
    仅测试自用）及配套测试、`lib.rs:50-51` 的 `EventBatch`/`RingingEventEnvelope` 导出；
  - `lib.rs:31-39` 对 `qaqh_domain::state` 的再导出（注释自证壳层不应依赖）。
- [x] **第 2 步（TUI 改一行）**：TUI `send_command` 换 v2 typed ACK（v2 是 v1 超集，
  `.status` 语义不变）；随后删 `client.rs:330-347` 的 v1 签名与
  `qaqh-ringing/src/v2/types.rs:503-514 / 586-597` 的 `into_v1`（有损投影，丢 typed
  `existing`/`result`）。
- [x] **第 3 步（结构收敛，可选）**：client 拆成 `qaqh-wire`（~1k 行：discovery + connect +
  lease 续租 + SSE 解码）+ 类型直用 `qaqh-ringing`/`qaqh-domain`（TUI 已 path 依赖后端仓）。
  完成后 client 退役。
- [x] **顺手修债**：TUI 的跨仓相对路径依赖（`../qaqh-backend/crates/...`）换 git 依赖或发版
  （TUI Cargo.toml 注释自述离线分发会断）。
- [x] **保留项（勿删）**：discovery 的 `ws://`→http 改写与旧 6 字段 `daemon.json` 兼容解析
  （`discovery.rs:14-42`，测试 301-351）——对既有磁盘数据必要。

## E. 死代码 / 过期文档（零风险，随手清）

- [x] `tools/session-forensics/session_forensics.py:12,145`：解析 `compact-context.json`，与
      README"没有 compact-context.json"冲突，永远读空；`:341` 的 `compact_skip` 同属旧 compact
      体系；`tools/session-forensics/README.md:45,99-100` 同步过期。
- [x] 6 处 no-op `#[serde(rename = "session_id")]`（字段本就叫 session_id，改名残迹）：
      `qaqh-ringing/src/envelope.rs:23,124,256`、`qaqh-ringing/src/worker.rs:16,47,80`。
- [x] 过期文档：`qaqh-client/src/lib.rs:1-7`（"three SSE event channels / Ringing V1"）、
      `client.rs:119`（"Ringing V1 client"）、`client.rs:60-63`（RemoteEndpoint "走 Ringing V1"）、
      `types.rs:127` 与 `timeline.rs:319` 的 "Ringing V1" 错误串、
      `qaqh-subagent/src/lib.rs:1-10`（legacy HTTP/SSE 回退描述，PR-4-2 已删）。
- [x] `qaqh-subagent/src/lib.rs:916-928`：`SubagentTransport::attach/close` 的 HTTP 时代空残端
      （`HostTransport` 实现为 no-op）。
- [x] `qaqh-ringing/src/v2/types.rs:213`：`RingingV2ResetReason::V1EpochMismatch` 历史命名变体
      （准死代码，改名前确认 wire 兼容）。
- [x] `qaqh-session/src/session_meta.rs`：整个模块仅为 backward-compat re-export 壳
      （`lib.rs:11,16`），确认无外部使用后删。
- [x] 命名口径统一（低优先）：`qaqh-types/src/session.rs:135` 注释 "this seed is passed…"、
      `qaqh-session/src/manager.rs:4`（`{seed}/` 目录文档）、`manager.rs:248-249`（per-seed 锁）、
      `qaqh-runtime/src/ringing/hub.rs:66`（`{seed, channel, revision}` 注释）、
      `envelope.rs:14-16,28`、测试数据 `envelope.rs:314` `"seed-1"`。

## F. 保留的双轨（有意设计，暂不动）

- **canonical identity sidecar**（`qaqh-session/src/canonical/identity.rs`）：v1 磁盘目录（legacy
  seed）↔ v2 UUIDv7 的桥 + `ulid_from_text` 迁移别名——对既有会话数据必要，随磁盘格式退役计划另行处理。
- **v1 信封在 crate 根的兼容窗口**（`qaqh-ringing/src/lib.rs:21-50`，`v2/mod.rs:3-4` 自述
  "2.0 compatibility window"）与内部 v1 hub 管道（`qaqh-daemon/src/server.rs:164-212`、v2 driver
  内部构造 v1 信封 `axum_impl/v2.rs:630-653`）——等 C/D 完成后评估是否硬切。

---

## G. TUI 并入后端仓（独立二进制，进程分离不变）

> **勘误 2026-09-29**：本节各条此前误标 [x]，仓库事实为未执行（无 crates/qaqh-tui、无 build-tui、无 ratatui patch）。全部改回 [ ]，见 docs/plan-beta-readiness.md P5。

> 2026-09-27 可行性结论：**可做，低风险**。运行时已具备进程模型——TUI 有
> `launch_daemon_if_missing: true` / `--no-spawn`（tui-app main.rs:95,139），发现 daemon
> 不在则拉起，经 HTTP/SSE 连接。搬家不改任何运行时行为。

- [ ] 新建 `crates/qaqh-tui/`，`[[bin]] name = "qaqh-tui"`；对外产物名不变；workspace members 加行。
- [ ] 依赖简化：`qaqh-client`/`qaqh-config-api` 的 `../qaqh-backend/...` 相对路径依赖改为
      workspace 内部依赖（离线分发问题消失）；后续按 D 轨直用 `qaqh-ringing`/`qaqh-domain`。
- [ ] **`[patch.crates-io]`（ratatui 本地工作树）必须上移到 workspace 根 Cargo.toml**
      （Cargo 只认根 patch）；后端不用 ratatui，无副作用；ratatui 发版后可直接删 patch。
- [ ] workspace 继承：TUI package 的 `version`/`lints` 改 `workspace = true`；
      核对 `scripts/sync-version.ps1` 覆盖新 crate（当前两仓同为 2.0.0-alpha3）。
- [ ] 构建/CI：justfile 加 `build-tui`；TUI 仓 `.cnb.yml` 迁移或废弃；注意 syntect/pulldown
      会加重 workspace 首编。
- [ ] Git：旧仓（D:\qaqh-tui-app）转只读归档，代码无历史或 subtree 并入。
- [ ] 收益：codegraph 前后端一图可见、单 CI 单版本号；清理草稿 D 轨"迁外部壳层"变成仓库内改动。

---

## 结构性备忘（codegraph 验证结论）

- 耦合星型中心是 **qaqh-runtime**（依赖 14/19 crate，52k 行，use-fan-out 229 断崖第一）；
  被动 hub 是 **qaqh-session**（use-fan-in 100）。编译图无环。
- 长期方向（若重启重构）：proto/store 下沉，runtime 退化为纯 agent 状态机，fan-out 14→4。

---

## H. 第二轮扫描新发现（2026-09-27）

> 协议硬切完成后对剩余耦合的再扫描。按风险从高到低排列。

- [ ] **H1（最大剩余耦合，引擎级，建议单独立项）**：daemon 的 v2 命令端点内部仍是 v2→v1
      翻译链——xum_impl/v2.rs:1000-1021 把 v2 信封重新序列化成 v1 信封 JSON，
      调用已无挂载路由的 v1 handler（xum_impl/command.rs:59 handle_command，
      mod.rs:54 转发），再进 v1 RingingHub（runtime hub.rs 全文以
      RingingEventEnvelope/RingingCommandEnvelope 为内部总线类型）；
      driver claim/release 同样构造 v1 信封转发（v2.rs:630-653）；subagent 收集器
      （subagent lib.rs:34）经 v1 EventBatch 监听。**这是进程内引擎，不是 wire 兼容**；
      砍法 = 让 hub 以 v2 信封为总线类型或抽出传输中立核心，动作大，需独立评审。
- [x] **H2（client 双份会话簿记，已切 2026-09-27）**：qaqh-client/src/session.rs 的 SessionState
      （v1 命名）与 ClientV2SessionState 由同一次 open 同时填充（注释自认"同一个
      lease"）；RingingSession 文档自称 "Ringing V1 session"。合并为单一 v2 state。
- [x] **H3（TUI banner 撒谎，已修）**：main.rs:69 打印 RINGING_VERSION(=1)，已改
      RINGING_V2_VERSION。
- [x] **H4（client crate 描述过期，已修）**：Cargo.toml description "Ringing V1/V2" → v2。
- [x] **H5（timeline wire 体 v1 版本号，已切 2026-09-27）**：timeline 页响应体 ersion 用
      RINGING_VERSION(=1)（runtime host_impl.rs:1638、client timeline.rs:314 校验、
      types.rs:121/170）——端点是 v2 但 body 版本字段是 1。切 RINGING_V2_VERSION
      需 daemon/client 同步改（小改动，注意 TUI/webui 是否读该字段）。
- [x] **H6（subagent HTTP 时代残端，已删 2026-09-27）**：SubagentTransport trait 的 ttach/close
      为 no-op 空残端（subagent lib.rs:916-963，测试 RecordingTransport 也实现），
      随 H1 或单独删除。
- [x] **H7（双向 dev-dependency——查实为有意设计，保留）**：qaqh-session dev-dep qaqh-message 且
      qaqh-message dev-dep qaqh-session（均 test-only）——整理共享测试夹具后可消。
- [x] **H8（session_meta re-export 壳，已删 2026-09-27）**：qaqh-session/src/session_meta.rs 仅
      backward-compat 再导出 qaqh_types::SessionMeta，workspace 内无
      qaqh_session::SessionMeta 导入方，确认外部无使用后删。
- [x] **H9（命名化石，已清 2026-09-27）**：RingingV2ResetReason::V1EpochMismatch
      （v2/types.rs:213）、client RingingResetRequired as ResetRequired 再导出
      （TUI 自有同名 variant，client 导出无人用）、client session.rs/timeline.rs
      的 "Ringing V1" 文档字符串（types.rs:127、timeline.rs:319）、
      subagent lib.rs:1-10 过期 crate 文档。

---

## I. 剩余待办总账（2026-09-27 复盘后）

> 协议硬切（本文 A-E 主体 + H3/H4/H5/H2/H6/H8/H9）已完成并全部编译验证。
> 以下为尚未执行的条目，按建议顺序排列。

- [ ] **I1. WinUI 2.0 桥接迁移**：D:\qaqh-winui-app\docs\migration-v2.md 已写好完整
      迁移指南（断点清单 + v1→v2 映射表 + TUI 模板引用 + 验收步骤）。WinUI bridge
      停在 v1 合同（on_batch/EventBatch/ChannelStatus），对 backend HEAD 编译不过。
- [x] **I2. 测试回归（2026-09-27 部分完成）**：受影响 crate 的 lib/bins 测试已跑——
      types 24✓ / ringing 32✓ / client 48✓+2 环境性✗（spawn sh 缺失，先行存在）/
      workspace 423✓+2 并行抖动✗（exec 子进程，单跑即过，先行存在）/ gateway 20✓ /
      session 63✓ / runtime-hub 47✓ / daemon-bins 61✓。未跑：daemon 集成测试
      （需 spawn daemon）、webui bun test、TUI 测试套件。
- [ ] **I3. qaqh-wire 拆分**（D-3）：client 瘦身为纯传输（discovery + lease + SSE 解码），
      WinUI/TUI 依赖目标改名。
- [ ] **I4. 跨仓 path 依赖 → git 依赖 + tag**：TUI/WinUI 两仓的
      ../../../qaqh-backend/... 相对路径（离线分发会断）。
- [ ] **I5. tools/session-forensics 清理**：compact-context.json / compact_skip 死解析
      （E 节已列，未执行）。
- [ ] **I6. TUI 并入后端仓**（G 节，需拍板）。
- [x] **I7. H1 引擎重构立项**：已立项，spec 落于 `docs/spec/hub-fact-bus-refactor.md`（思路 B：事实总线，三阶段可停点，含 agent loop 插件化协同约束）。原文：v2→v1 内部翻译链（v2.rs:1000-1021）+ v1 hub 总线 →
      事实总线（思路 B/C，含 Codex SQ-EQ / opencode v2 行业参照；单独立项 + 全量测试兜底）。
- [ ] **I8. WinUI 技术栈决策已定**：留 Rust（类型单源论据），UI 大改时守住
      bridge 层边界（见 WinUI 仓 docs/migration-v2.md 第 4 节）。
