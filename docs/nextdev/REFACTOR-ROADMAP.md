# DeepX 架构重构路线图（REFACTOR ROADMAP）

> **状态**：方向已定稿（2026-08-16）。本路线图只定**目标 + 路线 + 文件级改动清单**；
> BUG 现象/根因/证据见 [bugfix-1.1-nextdev.md](./bugfix-1.1-nextdev.md)；旧计划文档已归档至 [../backlog/](../backlog/)。
> 战略顺序：**砍架构 → 统一语义 → 再谈创新**。

---

## 1. 决策与背景

| 决策 | 内容 |
|---|---|
| **D1 保留多前端** | WinUI / TUI / Tauri / Web-BFF 多前端并存，daemon 是唯一服务器 |
| **D2 Rust 只做核心** | Rust 专注**后端核心 + 公共 SDK**；前端壳薄化，未来可用 C# 等重写（WinUI3 维护吃力不讨好，保留它不等于绑定它） |
| **D3 优先级** | 砍架构 → 统一语义 → 再谈创新 |
| **D4 deepx-client 转正** | 从“应退役”（BUG-010）改为**唯一公共客户端 SDK**，所有外部前端共用 |
| **D5 对外协议是资产** | daemon 的 Ringing V1 HTTP/SSE 契约**保留并现代化**，不是负债 |
| **D6 不做 engine 插件化** | 本地单机引擎集合固定，无需 TurnEngine/ToolEngine 可插拔；正确方向是“诚实模块化”（拆巨型函数 + 明确固定分发），而非造假 trait |
| **D7 做注入者插件化** | loop 只是“忠实流转器”；统一**注入插槽（InjectionBus）**，让 subagent / skills / MCP / 系统 / 用户插队问话 都经同一入口注入，loop 不关心谁注入 |
| **D8 最多改两处** | **下一版本工程宗旨**：任何新增/修改点**最多触及两处代码**（理想一处）。加工具只改注册聚合器一处；新增档位 / 工具名映射收进单一契约；多写入口、多映射、硬编码表都是要消灭的对象 |
| **D9 Windows 桌面层拆仓（已落地 2026-08-16）** | WinUI3 壳 / installer / updater + 4 个前端 crate（fluent / markdown-core / markdown-winui / app-notifications）迁至独立仓库 `deepx-winui-app`；主仓库只剩跨平台后端（18 crates + dsh-minimal-mode），Ubuntu 上 `cargo check/test --workspace` 全量干净。前端经 `path` 依赖消费后端 SDK：`deepx-client` / `deepx-update` / `deepx-types` |
### 影响
- `deepx-client` 从“计划退役”翻转为“扶正为正式 SDK”。
- daemon 网络层保留，但实现（手写 HTTP）要现代化。
- 单机复杂度按“本地优先 + 多前端”重新平衡：砍进程/持久化冗余，不砍对外契约。

---

## 2. 目标形态（多前端）

```
deepx-core（单体：daemon + agent loop + 子代理 actor + 事件溯源存储 + 薄 HTTP/SSE 层）
   │
   └── Ringing V1（现代化契约 · 单一权威持久化 · 统一语义）
        ├── WinUI 原生壳（薄 bridge，未来可换 C#）
        ├── TUI          （deepx-client 直连 daemon）
        ├── Tauri        （Rust 后端 deepx-client + IPC，Web 前端经 IPC）
        └── Web          （BFF 同源代理 → deepx-client）
```

进程：N 个 → **1 个 daemon**。
持久化：4 套 → **1 条事件溯源日志**（其余为可重建投影）。
crate：碎片 → **core 单体 + 公共 client SDK + 薄前端壳**。

---

## 3. 阶段一：砍架构（P0，先动最重的）

### 刀 1：子代理 in-process 化，daemon 吃掉 agent
- **目标**：本地单机不再多进程 + 三层连接。
- **砍**：`deepx-daemon`（服务器）独立 `agent worker` 进程、子代理完整进程 → 合并为一个 daemon 进程内的 **主 agent loop + N 子代理 actor**（channel / 进程内事件总线）。
- **证据**：`deepx-runtime/src/registry.rs:197-330`、`worker.rs:1-140`、`deepx-subagent/src/lib.rs:1-77`、`deepx-runtime/src/service.rs:590`。
- **对应 BUG**：002、011。
- **风险**：并发隔离变弱 → 靠 workspace 边界 + 权限级别守住（安全本就不靠进程隔离）。
- **判据**：`spawn_subagent` 不再 `Command::new(exe)`；无三层连接；单进程可跑主 + N 子。

**已落地 step 1（2026-08-17）**：
- `LoopChannels` / `Loop::from_channels` 把 Ringing loop 从 OS pipe 抽象为进程内 channel；
- `AgentRegistry::spawn_subagent` 改为 daemon 线程内 Ringing Loop actor，事件直接进 `RingingHub`；
- 子代理 actor 使用私有 ToolManager，`process kill/check` 与关闭路径兼容。
- 明确边界：`qaqh-workspace` 仍为进程级全局状态，因此 actor 由 `SUBAGENT_ACTOR_SERIAL` 串行；主 agent 仍是独立 worker 进程。step 2 继续做逐 actor 上下文、主 loop 入 daemon、`qaqh-subagent` 宿主直连。

### 刀 2：持久化收敛为“一条日志 + 投影”
- **目标**：单一权威数据源，其余全为投影。
- **砍**：`timeline` / `messages.jsonl` / `compact-context` / `ringing journal` 四写 → **一条 append-only 事件溯源日志**；timeline、消息、compact 均为可重建投影。
- **证据**：`deepx-runtime/src/timeline_store.rs`、`ringing/hub.rs:843-872`、`deepx-session/src/manager.rs:133-142,145-190,450-550`、`deepx-message/src/store.rs`、`ringing/journal_store.rs`。
- **对应 BUG**：005、006、007。
- **风险**：历史数据迁移（一次性迁移脚本 + 投影重建）。
- **判据**：删除任一投影可重建；无“四套互不收敛”。

### 刀 3：传输实现现代化（协议语义保留）
- **目标**：砍掉手写 HTTP 的维护与 I/O 放大。
- **砍**：`ringing_http.rs`（107KB）手写解析/序列化 → **axum/actix** 或至少拆模块。**保留** Ringing 的 ack + 批量事件 + SSE + lease 语义。
- **证据**：`deepx-daemon/src/ringing_http.rs`（107KB）。
- **对应 BUG**：011（同步 timeline 持久化 / 手写 HTTP）。
- **风险**：低，行为对齐测试兜底。
- **判据**：HTTP 层用成熟框架；行为测试全过。

---

## 4. 阶段二：统一语义（P1，收敛契约与写口）

### 刀 4：crate 边界重划 + deepx-client 转正
- **目标**：消除“为拆而拆”的碎片，形成单一公共客户端 SDK。
- **砍**：`deepx-ringing` / `deepx-domain` / `deepx-message` / `deepx-session` 边界重新划分；`deepx-client` **转正为唯一公共 SDK**（TUI/Tauri-Rust/Web-BFF 全走它）。
- **证据**：`docs/backlog/` 下旧收敛计划；BUG-009（插件化 loop 名存实亡）、BUG-010（碎片化）。
- **对应 BUG**：009、010。
- **风险**：中，依赖树重构。
- **判据**：一个改动不再横跨 5+ crate；外部前端只有一个连接 SDK。

### 刀 5：前端 bridge 拆解 + 清 Web 残留
- **目标**：壳薄化、职责清晰。
- **砍**：`bridge.rs`（262KB）按领域拆文件；删 89 处“对齐 Web”注释；`shell.* 回传写路径` → 直发 daemon；`poll_rev` 多 timer → 单一 `CompositionTarget::Rendering` 泵。
- **证据**：`apps/winui/src/bridge.rs`、前端 Web 迁移残留审计（bugfix 内）。
- **对应 BUG**：001、前端审计。
- **风险**：低，纯重构，winui 现有测试兜底。
- **判据**：bridge 拆分为多模块；无 Web 残留注释；单一 UI 泵。

### 刀 6：配置单写口 + 档位单一契约
- **目标**：设置与工具模式只有一个事实源。
- **砍**：`config.save` / `config.set_permission_level` / `ui-preferences.json` / 本地 theme 四条写路径 → **统一 patch API + 单写口**；`tool_mode` 档位 7 处硬编码 → **单一契约表**。
- **证据**：`deepx-config/src/config.rs`、`deepx-runtime/src/service.rs:464-495`、`apps/winui/src/bridge.rs`；BUG-001/008/013。
- **对应 BUG**：001、008、013。
- **风险**：低。
- **判据**：设置/工具模式只有一个写入口；新增档位改一处。

**已落地（2026-08-16，后端）**：`qaqh_config::Config::update` 单写口 + daemon 六个 config 写路径收敛；
`tool_mode` 档位契约已由 `qaqh_types::tool_mode` 承载。剩余为 WinUI 本地偏好路径与工具名别名通用化。

### 刀 6B：tool 层统一注册 / 通用别名 / 折叠归属声明
- **目标**：新增/修改一个工具**最多改一处**（符合 D8）。
- **砍**：
  1. **统一注册聚合器**：`agent.rs` `init`/`init_subagent` 手动列 `&[deepx_subagent::register, dsh_minimal_mode::register]`（加工具改两处）→ 建 `tool_registry::all_registrars()`，加工具只改一处。
  2. **通用工具名别名**：本次 `minimal:dsh` 的 `bash_v2→bash`（模型面投影 + 执行面 normalize 特判）→ 提升为 ToolManager 通用能力（`ToolHandler` 声明 `model_name`，`filtered_defs` 投影模型面、`prepare_req` 转内部 key）。改名 / 换执行通用，不再各模式特判。
  3. **折叠归属声明**：`tool_side_fold::StandardPolicy` 中央硬编码“工具名→折叠类”表 → `ToolHandler` 自带 `FoldClass` 声明，`StandardPolicy` 从 handler 读，删中央表。新增工具自动获得正确折叠。
  4. **handler 构造 helper**：减少各工具包手写 `ToolHandler{...}` 样板。
- **证据**：`crates/deepx-workspace/src/registration.rs`（build_tool_manager）、`crates/deepx-msglp/src/state/agent.rs:325-329,337-350`、`crates/deepx-workspace/src/tool_side_fold.rs`；`dsh-minimal-mode` 注册 `bash_v2`。
- **对应成果**：`minimal:dsh` 已落地（见下）；为未来改名 / 换执行铺路。
- **风险**：低-中（②③ 触及 ToolManager 核心，需全量工具测试兜底）。
- **判据**：加一个工具包只改一处；模型面改名不触碰各模式特判；新增工具无需改折叠中央表。

### 已落地（2026-08-16）：minimal:dsh 极简模式
- **目标**：完全对齐 deepseek-harness minimal preset（`F:\deepseek-harness-winui\minimal-mode-extraction`），触发模型“最大化思考”。
- **实现**：新增 `minimal:dsh` 档位（复用 NoFold）；白名单 `[bash_v2, str_replace_editor]`；模型面投影 `bash_v2→bash`（逐字 schema）；执行面 `admit_batch` 别名路由回 bash_v2（原生 bash 绕过）；系统提示覆盖为唯一一句 `You are a helpful software engineer assistant.`；跳过 skills 注入；前端 composer 加“极简”选项。
- **验证**：tool_schema 精确 {bash, str_replace_editor}、内部 bash_v2 不泄漏、normalize 别名、prompt 逐字测试全过；现有工具模式测试 + winui check 无回归。
- **意义**：为“工具名别名通用化”（刀 6B-②）提供真实落地用例；`dsh-minimal-mode` crate 忠实复刻 bash_v2 + str_replace_editor。

### 刀 7：统一注入插槽（InjectionBus）——让 loop 只做忠实流转
- **目标**：subagent / skills / MCP / 系统 / 用户插队问话 都走**同一个注入入口**，loop 不再被各身份的注入特判塞满。
- **现状（问题）**：注入路径分散——subagent 走 `engine_input.rs:219` 系统注入 + `loop_core.rs:483 drain_pending_injections`（“见缝插针”）+ compact 期间 `deferred_ringing` 特判（L1039-1059）；skills 直接 `queue_request`（L1127）；用户走专属 `handle_user_input`。每个身份各自回答“注入到哪/时机/队列”，答案不一致；MCP、插队问话若新增又将各发明一套。
- **设计**：`loop.inject(Injection{ source, role, text, meta.command_id, priority(Normal|Deferred|Preempt), semantics(NextTurn|Inline|Interrupt) })` 为**唯一注入入口**；loop 集中做 ①统一队列（priority+FIFO）②时机决策（有无 turn / 是否 compact）③幂等去重（command_id）。身份只构造消息，不再碰 pending/deferred/compact。
- **效果**：“插队问话”= `priority: Preempt, semantics: Interrupt` 的注入，无需新特判；MCP / skill 作为新 `source` 直接接入。
- **对应 BUG**：与 BUG-009（dispatch 硬编码）同源；为 subagent/skills/MCP/插队铺路。
- **风险**：中——时机/优先级语义要一次定对（这是“统一语义”的核心）。
- **判据**：所有身份只调 `loop.inject()`；`drain_pending_injections`/`deferred_ringing`/compact-defer 收敛为统一策略；MCP 与插队问话无需改 loop 即可接入。

### loop 架构规范（刀 7 基准，2026-08-16）

**定位**：loop = **编排层**。不做“纯转发”（它必须承载状态/调度/注入/发射），也不做“业务巨石”（业务归各身份 / 工具侧）。它把「数据流主干」与「编排职责」收敛为一个清晰、可独立测试的边界。

**数据流主干**（agent 主循环，gate→tools→repeat）：

```
用户input ──► [loop 组装 messages + tools_schema] ──► API(模型)
工具 output ─► [loop ◄── 反序列化响应（流式 / 工具调用）]
                    │ 解析出 tool_calls → admit(权限) → 分发给工具侧
                    └──► 工具侧（workspace：in-process 为主）
```

主循环：收 API → 反序列化 → 有工具调用则 admit + 分发 → 收工具 output → 回填 → 再发 API，直到无工具调用才输出终答。

**loop 的 5 项职责边界**：
1. **数据流主干**：收 / 反序列化 / 组装 / 发送（上面的主循环）。
2. **状态与上下文**：turn 状态机、消息历史、compact 时机。
3. **InjectionBus**：subagent / skills / MCP / 系统 / 用户插队 统一注入，按 `priority(FIFO|Deferred|Preempt)` + `semantics(NextTurn|Inline|Interrupt)` 调度。
4. **权限 admit**：工具调用在执行前过准入（自动批准 / 询问 / 拒绝）。
5. **事件发射**：流式 token、timeline、工具生命周期推给前端。

**工具执行边界**：默认 **in-process**（同进程函数调用 / 进程内 IPC）；**HTTP/WS 网络边界只出现在外部工具服务**（MCP 服务器、远程工具、跨进程子代理），不放在 loop↔工具侧之间。

**明确不做的**：engine 插件化（D6，本地单机无需可插拔 Turn/ToolEngine）；loop 不承载各身份的业务特判（归 InjectionBus / 各身份自身）。

---

## 5. 阶段三：再谈创新（P2，多前端协议扶正）

### 刀 8：对外协议扶正 + 多前端适配
- **目标**：把 HTTP/SSE 契约变成稳定、可版本化、多前端友好的正式协议。
- **做**：契约版本化；**TUI 直连 daemon**（读本地 token）；**Tauri/Web 走 BFF 同源代理**（token 不暴露给浏览器）；daemon 增加浏览器友好鉴权（短期 token / cookie）供 Web 直连形态。
- **证据**：`deepx-client`（纯 Rust 无 GUI，天然供 TUI）；README `/debug/` 静态服务先例。
- **对应 BUG**：012（已 fixed 的白名单类摩擦归类到这里统一治理）。
- **风险**：中，需先定鉴权模型。
- **判据**：TUI 能 `deepx-client` 直连；Tauri/Web 经 BFF；daemon 契约有版本号。

---

## 6. 执行策略（避免大重构翻车）

1. **顺序依赖**：刀 2（持久化）先行——它是数据正确性地基；再刀 1（进程）→ 刀 3（传输）→ 刀 6+刀 6B（配置 + tool 层统一）→ 刀 7（注入插槽）→ 刀 5+刀 4（前端 + crate 边界）→ 刀 8（多前端）。
2. **宗旨（D8）**：任何新增/修改**最多改两处**（理想一处）。多写入口、多映射、硬编码表都是要消灭的对象；先收敛成单一契约，再谈新功能。
3. **每刀带测试**：每刀完成即跑对应 crate 全量测试 + 回归；纯重构（刀 3/5/6/6B）靠现有测试兜底。
4. **决策门**：阶段三（刀 8）前必须拍板“TUI/Tauri/Web 各自连接形态与鉴权模型”。
5. **不留半成品**：一刀未达“判据”不进入下一刀；可回滚点每刀一个。

---

## 7. 相关文档

- Bug 档案（现象/根因/证据）：[bugfix-1.1-nextdev.md](./bugfix-1.1-nextdev.md)
- 待办/已归档旧计划：`docs/backlog/`（本次归档 31 份：PLAN-*、winui-*、协议/子代理/工作流等）
- 保留：`docs/nextdev/legal/`（用户协议/隐私政策，非待办）
