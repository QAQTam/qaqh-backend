# 1.1 NextDev 探索与重构执行说明（2026-08-16 第二轮）

> 本文件是对 `REFACTOR-ROADMAP.md` 与 `bugfix-1.1-nextdev.md` 的**落地校准**：
> 路线图写于 `deepx-*` / WinUI 时代，当前主仓库已完成 crate 更名（`qaqh-*`）
> 与桌面壳拆仓（D9）。本文档把路线图映射到当前真实代码，并给出
> keep / remove / merge 清单。代码改动随 PR 逐步落地，本文件随每刀更新。

---

## 1. 探索结论：文档与仓库的差异

| 文档中的路径 | 当前真实路径 | 结论 |
|---|---|---|
| `crates/deepx-*` | `crates/qaqh-*` | 纯改名，结构映射 1:1 |
| `apps/winui/...` | 已拆至独立 `qaqh-winui-app` 仓库 | D9 已落地，BUG-001/003/004 的 WinUI 侧代码不在本仓库 |
| `apps/tui` | `apps/tui`（qaqh-tui，锁定 `minimal:dsh`） | 保留，是 `qaqh-client` 的第一消费者 |
| 文档未收录的 `apps/web` | `apps/web`（SolidJS Web 壳） | 现状新增多前端，D1 覆盖它 |
| `deepx-client` 应退役 | `qaqh-client` 已是 HTTP/SSE SDK | D4 已事实上转正，文档需追认 |

索引工具：`codegraph status`（226 文件 / 5349 节点 / 19461 边）可用于继续做
符号边界分析；`cargo check --workspace` 当前通过。

---

## 2. 根因压缩（BUG-001 ~ BUG-013）

| Bug | 根因一句话 | 当前状态 |
|---|---|---|
| 001 | 设置 4 条写路径，无单写口；Web 时代状态归属注释残留 | 后端 fixed：daemon 六个 config 写路径收敛到 `Config::update`；WinUI 本地 `ui-preferences.json`/theme 仍在 `qaqh-winui-app` |
| 002 | 子代理 = 完整 `qaqh agent --seed` 子进程 + 三层连接 | 部分 fixed（刀1 step1）：`spawn_subagent` 改 daemon 内 Loop actor；主 loop 入 daemon 与宿主直连待 step2 |
| 003 | vsync 泵全量 clone + 无 generation 互斥 | fixed（修复在已拆出的 WinUI 仓库） |
| 004 | 单槽 timeline 无 active 守卫 + offset 恢复语义 | fixed（qaqh-winui-app #1）：viewport 恢复移除、恢复即贴底；active guard/resume generation 已在 WinUI 仓库修复 |
| 005 | timeline / messages.jsonl / compact-context / journal 四套持久化互不收敛 | open |
| 006 | `handle_timeline_snapshot` 无 messages→timeline 回退重建 | fixed：#4 `timeline_rebuild` 缺失/损坏时从消息重建并写回 |
| 007 | compact 损坏/缺失时 `unwrap_or(archive)` 回退全量，compact 可逆 | fixed：#5 损坏/越界 fail-closed，不再回退 archive |
| 008 | `Config` 只有 load/save 值对象，无全局缓存与 patch API | fixed（后端）：#6 `Config::update` 单写口 + daemon 六写路径收敛 |
| 009 | 注释宣称 Engine trait 可插拔，实际 `dispatch_ringing_one` 硬编码 | 本 PR 清理误导注释，实质重构入刀 7 |
| 010 | crate 碎片化 + 大文件（ringing_http/hub/engine_turn/store） | open |
| 011 | 子进程成本 + 多份双写 + std Mutex 内同步 I/O + 手写 HTTP | 部分 fixed（刀1 step1）：子代理子进程/三层连接消除；双写/同步 I/O/手写 HTTP 仍待刀 2/3 |
| 012 | HTTP action 白名单漏放 `session.set_tool_mode` | fixed，`is_allowed_action` 已含 |
| 013 | 档位表 7 处硬编码（4 文件） | 本 PR 收敛为 `qaqh_types::tool_mode` 单契约 |

---

## 3. Keep（保留什么）

| 资产 | 原因 |
|---|---|
| **Ringing V1 契约**（`qaqh-domain` + `qaqh-ringing` + HTTP/SSE 语义） | D5：对外协议是资产，保留 ack/batch/SSE/lease/cursor |
| **`qaqh-client` SDK** | D4：所有外部前端唯一连接入口；TUI/Web/WinUI 共用 |
| **`qaqh-msglp` loop 本体** | 编排层边界正确：数据流主干、状态、注入、admit、事件发射 |
| **`qaqh-workspace` 工具库** | 业务价值最重的工具执行层，in-process 为主；只拆大文件，不拆 crate |
| **`qaqh-types`** | 跨 crate 基础契约；本次起承载 tool-mode 等单契约表 |
| **`qaqh-gate`** | 模型网关独立正确，OpenAI/Responses 适配继续保留 |
| **`qaqh-config`** | provider registry / prompts / secrets 迁移有价值；补统一 patch API |
| **`qaqh-session` + `qaqh-message` 语义** | 会话元数据与消息状态机是正确概念，保留概念、合并存储实现 |
| **`apps/tui` + `apps/web`** | 多前端事实消费者，薄壳方向保留 |
| **`dsh-minimal-mode`** | 极简工具包有真实对齐价值；注册入口并入聚合器 |
| **测试与行为回归** | 每刀以现有 crate 测试为基线 |

---

## 4. Remove（删除什么）

| 删除对象 | 位置 | 对应 |
|---|---|---|
| 子代理进程模型 | `qaqh-runtime/src/registry.rs spawn_subagent`（step1 已删进程路径；`qaqh-subagent` 的 daemon/SSE 回连与主 worker 进程待 step2） | BUG-002/011 刀1 |
| timeline / messages / compact / journal 四写 | `timeline_store.rs`、`hub.persist_timeline_sync`、`manager.save_append`、`journal_store.rs` 多主逻辑 | BUG-005/006/007 刀2 |
| 手写 HTTP 解析/序列化主体 | `qaqh-daemon/src/ringing_http.rs`（约 110KB） | BUG-010/011 刀3 |
| 假 Engine 插件注释与 `engines vec` 指引 | `qaqh-msglp/src/ringing_v1/mod.rs`、`lib.rs` | BUG-009（本 PR 已清理） |
| 直接 `Config::load() -> mutate -> save()` 写口 | `qaqh-runtime/src/service.rs` 各 action、`qaqh-msglp/state/agent.rs` 等调用点 | BUG-001/008 刀6 |
| 工具档位硬编码表 | `qaqh-msglp/state/agent.rs`、`qaqh-runtime/service.rs`、TUI 字面量 | BUG-013（本 PR 已收敛） |
| Web 时代残留命名/注释（文档层） | 两个 nextdev 文档里的 `deepx-*`/`apps/winui` 旧路径 | D9 追认 |

---

## 5. Merge（合并什么）

按依赖方向从外向内合并，目标是“外部前端只认识 3 个 crate：
`qaqh-client` + `qaqh-types` + 协议/领域类型”。

| 合并动作 | 现状 | 目标 |
|---|---|---|
| `qaqh-runtime` + `qaqh-daemon` + `qaqh-subagent` | daemon 依赖 runtime，HTTP 在 daemon、hub/service 在 runtime，子代理再经 client 回连 daemon | **`qaqh-core`**：一个常驻进程，主 loop + N 个 in-process subagent actor + 事件溯源存储 + 薄 HTTP/SSE |
| `qaqh-session` + `qaqh-message` + `timeline_store` + `journal_store` | 四个 crate/模块各自落盘 | **`qaqh-store`**（或并入 core）：一条 append-only 事件日志，messages/timeline/compact 均为投影；SMJ 编辑日志同源接入 |
| `qaqh-proto` 的投影模型 | `TurnData/RoundData/RoundBlock` 等仅供会话投影 | 并入 `qaqh-domain`（领域投影）或 `qaqh-store`；删除独立 proto crate |
| `qaqh-ringing` | wire 层被 daemon/client/worker 共享 | 保留 wire 层，但对外只经 `qaqh-client` 重导出；不新增第二套客户端协议 |
| 工具注册入口 | `init` / `init_subagent` / 测试各手写 `[qaqh_subagent::register, dsh_minimal_mode::register]` | `agent_tool_registrars()` 聚合器（本 PR 已落地） |
| 档位契约 | 7 处硬编码 | `qaqh_types::tool_mode` 单契约（本 PR 已落地） |
| `qaqh-gate-testui` | 独立 workspace 成员 | 移入 `tools/` 或保留为 dev target，不参与核心依赖图 |
| `qaqh-update` | 后端 update/package 引擎 | 确认 winui 仓库消费边界后决定去留；暂保留为公共 SDK |

---

## 6. 落地顺序（相对当前仓库）

1. **刀 6/6B 的纯契约部分**（低风险）：tool-mode 单契约、注册聚合器、假插件注释清理。← #3 已落地。
2. **刀 2**（数据地基）：SMJ 编辑日志已落地（#8）；timeline 重建（#4）与 compact fail-closed（#5）已落地；四套持久化收敛为一条日志仍待做，先写迁移与重建测试。
3. **刀 1**：subagent in-process actor step 1 已落地（`spawn_subagent` 不再 `Command::new(current_exe)`）；step 2 继续主 loop 入 daemon、逐 actor 上下文、`qaqh-subagent` 宿主直连。
4. **刀 3**：ringing_http 换 axum（或先按端点拆模块），行为测试全量兜底。
5. **刀 6 剩余**：`Config::update` 单写口已落地；剩余为 `qaqh-subagent`/TUI 等外围调用点与 WinUI 本地偏好路径。
6. **刀 7**：InjectionBus 统一 subagent/skills/MCP/系统/用户注入。
7. **刀 4/5/8**：crate 边界合并、多前端协议版本化、鉴权模型拍板。

每刀判据、风险和验证以 `REFACTOR-ROADMAP.md` 第 3-6 节为准；判据未达成不进入下一刀。
