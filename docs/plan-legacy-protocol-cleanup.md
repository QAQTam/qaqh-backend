# 过时协议清理计划（legacy-protocol-cleanup）

> 状态：规划稿，2026-10-04。基准提交 `76f804e`（GitHub `origin/main`，唯一真源）。
> 依据：本会话落地"写工具参数流式行数估算"时对接线面的逐项 `file:line` 复核，
> 加上同日 cnb → GitHub 迁移时的分叉盘点。
> 本文只列**可以删 / 必须接 / 必须改名**三类过时面，不含新功能。
> 每条都带证据；没有亲自复核过的都标了「待复核」，别当结论用。

## 0. 迁移基线（先记事实，后面所有清理都以此为底）

- cnb 远端已从两仓移除，唯一 remote = GitHub：
  `E:/qaqh-backend` → `github.com/QAQTam/qaqh-backend.git`，
  `E:/qaqh-tui-app` → `github.com/QAQTam/qaqh-tui-app.git`（tui 已 FF 同步）。
- 后端 main 以 `--force-with-lease` 强推完成：`56d79da...76f804e (forced update)`。
  远端**没有任何 tag**（`git ls-remote` 只返回两个 head），故不存在悬空 tag。
- 被丢弃的那条线（12 个提交：9× `refactor(identity)` + 4× docs，83 文件）只活在本地分支
  `preserve/github-identity` = `56d79da`，对象已在本地对象库、可解析、可 cherry-pick。
- `refactor/tool-result-projection`（`54f391b`）经核实是 `HEAD` 的**祖先**，无独有工作，
  远端原样保留。
- 后续账：其它机器/其它 clone 现在与远端分叉，需要 `git fetch && git reset --hard origin/main`；
  本仓的 worktree 分支 `feat/workspace-audit-pr3`（`ee3ba22`，codex worktree）基于旧线，
  要 rebase 到新 main 才能继续。

## 1. 已经干净的部分（清理工不许误伤）

| 事实 | 证据 |
|---|---|
| 生产代码不再把 `seed` 当序列化字段 | 排除测试后全仓仅两处，且都是 legacy 读取的测试夹具：`crates/qaqh-runtime/src/ringing/service_methods.rs:156,161` |
| 请求参数已是 `session_id` | 客户端**反向断言**不带 seed：`crates/qaqh-client/src/endpoint.rs:454,493` |
| 会话目录已是 `sessions/{session_id}` | `crates/qaqh-runtime/src/agent/types.rs:525`（`sessions_dir().join(session_id)`） |
| legacy 兼容件已删 | `generate_seed` / `legacy_seed` / `.legacy-session-ids.json` 全仓 0 文件命中 |
| v1 三频道 SSE 已硬切 | `crates/qaqh-daemon/src/axum_server.rs:627` 注释；路由表全是 `/ringing/v2/*`（`axum_server/axum_impl/mod.rs:145-177`） |

结论：`seed → session_id` 这件事**对外契约层已经做完**（而且比被丢弃那条线更彻底——
那条线在 `session/manager.rs` / `axum_server.rs` 上还留着兼容读取路径）。剩下的都是内部命名。

## 2. 待办 A：内部标识符改名（纯命名，零 wire 影响）

本地生产代码仍有 `seed` 标识符约 700 处（含注释）。热区与对照（同一文件两线的出现次数）：

| 文件 | 本地 | 被丢弃线 |
|---|---|---|
| `qaqh-runtime/src/ringing/hub.rs` | 66 | 0 |
| `qaqh-session/src/manager.rs` | 61 | **232** |
| `qaqh-daemon/src/axum_server.rs` | 56 | **77** |
| `qaqh-runtime/src/registry.rs` | 35 | 0 |
| `qaqh-runtime/src/activity.rs` | 12 | 0 |
| `qaqh-client/src/endpoint.rs` | 6 | 0 |
| `qaqh-runtime/src/ringing/service_methods.rs` | 5 | 0 |
| `qaqh-runtime/src/agent/paced_emitter.rs` | 2 | 0 |

- [ ] 路线 1（回收）：cherry-pick `preserve/github-identity` 上的 9 个 refactor，
      **逐文件核 diff**。禁止整串照收：那两个"本地更少"的文件说明对方线上有我们已经
      删掉的死码，整收会把它带回来。
- [ ] 路线 2（自扫）：按对方那份规格本地重做。规格在被丢弃分支上，取用：
      `git show preserve/github-identity:docs/current/spec/2026-09-25-session-identity-unification.md`
- [ ] 建议顺序：先做小的（`activity.rs` 12 / `paced_emitter.rs` 2 / `service_methods.rs` 5）
      把流程跑通，再动 `hub.rs` 与 `registry.rs`。
- [ ] 收尾门（两条路线都要）：`just ts-export && just ts-check` + `cargo test --workspace`。
      改名一旦触到 `derive(TS)` 的类型，`webui/src/api` 会跟着漂移，`ts-check` 是看门人。

## 3. 待办 B：设计好但没接线的通道（这才是"过时协议"的主体）

| 通道 | 现状（证据） | 决策 |
|---|---|---|
| `ToolEvent::ToolCallPrepared { args_so_far }`（`qaqh-domain/src/event.rs:457-463`，分类 `:531` Replaceable） | 每个 SSE 帧发一条**整段** args；`qaqh-runtime/src/ringing/projection.rs` 里没有它的分支 → 到不了 UI | [x] **已删（2026-10-05，随 P1.3）**：发射点 `turn_lap/gate.rs:679-688` 与变体一起摘掉。删前普查：生产侧无人匹配 `ToolCallPrepared`（桥只匹配 `ToolPermissionRequested`/`Started`/`Finished`/`Notice`），`tool_call_prepared` 字面量在 crates/webui/scripts/docs/数据目录 0 命中，`DomainEvent`/`ToolEvent` 无生产反序列化点 → 不影响历史账本重放。「接进投影」这条选项已随 v1 快照投影退场而不成立。 |
| `ToolEvent::CodeChanged`（`event.rs:519-520`） | 生产侧已完整（本次补齐 `write`/`apply_patch` 口径），但投影丢弃，只落 `code_stats.jsonl`（`agent/types.rs:518-542`） | [x] **已删（2026-10-05，随 P1.2 第一批 `065f35a`）**：UI 的权威行数另有两条来源且都已接线（流式 `TimelineIntent::ToolEstimated` → `reducer.ts:326`；终态 `display.lines_added/removed` → `StepRow.tsx:75,163`，`diff/parse.ts:7` 早已停止前端自算），本事件无人匹配、webui 0 引用；`code_stats.jsonl` 是 `ctx.stats.push_delta` 的**独立路径**（`service/stats.rs` 只读它的 `file` 字段），删事件不删统计。钉 legacy 形状的测试随宿主一起删——生产无 `ToolEvent` 反序列化点，能喂那个形状的只有测试自己。 |
| `webui/src/api/qaqh/*.ts` 零消费者生成物 | ts-rs 全量导出，前端实际只用 timeline 一条通道 | [x] **已收窄（2026-10-05）**：导出面按「前端 import 闭包 ∪ 仍在序列化上线的契约」白名单化，生成物 186→132，砍掉 54 个 fire-into-void 类型（`DomainEvent`/`ToolEvent`/`ControlEvent`/`ConversationEvent`/`RingingEvent`/`ControlState`/`ConversationState`/`ToolState`/`DomainCommand`、v1 `ToolResult` 信封族、`Dashboard*`/`Skill*`/`PlanReviewItem`…）。`Projection*` 与入站 `*Command` **保留**——SSE v2 与 bootstrap 仍在序列化它们，只是前端按 untyped envelope 消费（转正前先别删）。 |
| `qaqh_gate::StreamEvent::ToolCallProgress { args_so_far }`（`qaqh-gate/src/message_api.rs:511`、`chat_completions_api.rs:347`） | 每帧 `.clone()` 整段累计串 = O(n²) 基座。58 KB 参数约 2493 帧；估算器已按字节偏移绕开它，但基座没修 | [x] **已改成携带片段（2026-10-05）**：三适配器全部改发 `args_chunk`（Chat Completions / Messages 发 provider 的增量，Responses 一帧给完整参数即整段）。**没有**同时携带累计长度——普查后消费面只有 `ArgLineSlot`（running counter，自己就能数）与首帧 `args_json`，加 `usize` 就是没人读的字段。`ArgLineSlot` 的字节偏移重算与 resync 兜底随之删除，换成「认出写工具前暂存片段（上限 16 KiB）」，`late_name` 语义不变。 |

## 4. 待办 C：口径与命名撞车

- [ ] `ToolCapabilities.streaming`（`qaqh-workspace/src/tool_capabilities.rs:12-13`）的语义是
      "执行期是否产进度帧（现状仅 exec）"，与**参数**流式无关。改名 `progress_frames` 或在注释里钉死。
- [ ] 行数**三口径**并存：参数行数（`code_delta.rs`、`arg_estimate.rs`）≠ 展示 diff 行数
      （`webui/src/diff/parse.ts`，前端自算，[契约缺口 D-1] 已注明）≠ 实际落盘改动。
      UI 现在靠"虚线 + 角标 → 终态强制替换"遮罩。若要统一，先定义谁是权威。
- [ ] `docs/spec-file-mutation-delta-v2.md` 里的 `FileMutationDelta` / `HunkDelta` /
      `ResolvedHunk` / per-hunk `on_hunk` 是**零实现**（`crates` + `webui/src` 全检无命中）。
      要么开工要么归档，别让规格与代码长期分叉。

## 5. 待办 D：发布文档与现实不符

- [ ] `docs/plan-beta-release.md` §2 把 `2.0.0-beta.2` 定义成"发布后热修号"，实际已用作主线版本
      → 改写该句，或改用 `beta.2.1` 这类点号做热修。
- [ ] `architecture-report.md:2303-2312` 那张"版本漂移表"已过期（`version.txt` 与交付面本次已对齐）。
- [ ] 本仓没有 tag；beta.1 的记录在 `qaqh-tui-app` 仓（`docs(release): v2.0.0-beta.1`）。
      要不要在本仓补 `v2.0.0-beta.1` 与 `v2.0.0-beta.2`（后者应打在 `76f804e`）。
- [ ] `qaqh-backend.lock.json` 仍 `2.0.0-alpha1` 且带 `git_commit` 字段；无代码消费它
      （只有 docs 提及），定 tag 时一并回写。
- [ ] `webui/pnpm-lock.yaml` 处于未跟踪状态，而 `just webui-build` 走 `bun install --frozen-lockfile`
      → 锁文件策略二选一（收 pnpm 锁并改 justfile，或删除它继续 bun 锁）。

## 6. 一页 checklist（勾完即清理结束）

- [ ] 第 2 节选定路线并完成改名，`cargo test --workspace` 绿
- [ ] `just ts-export` + `just ts-check` 无漂移
- [ ] 第 3 节四条通道逐条表态（删 / 接），代码里没有"发了但没人收"的事件
      （P1.2 第一批已删 10 变体 + 3 访问器 `065f35a`；剩余见交接文档 §三）
- [ ] P2 各项动刀前先跑 `scripts/v2-legacy-compat-probe.sh`（探针已建 `aabfccd`，
      本机 13/13 零命中；beta 用户存量要另跑或按版本跨度确认）
- [ ] 第 4 节三口径至少在文档与注释里说清权威是谁
- [ ] 第 5 节四条发布事实更正
- [ ] `feat/workspace-audit-pr3` worktree rebase 到新 main
- [ ] `preserve/github-identity` 的处置：回收完毕删除，或明确长期保留并写清用途
