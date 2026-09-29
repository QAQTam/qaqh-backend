# Beta 发版前计划（beta-release）

> 状态：规划稿，2026-09-29。目标：完成"从当前 HEAD 到 beta 发布"的全部动作。
> 前提：`docs/plan-beta-readiness.md` 第 0/1/2 节已全部完结（G1-G5 门禁 + W1-W6 窗口项，
> 记录见 `docs/handoff/beta-readiness-2026-09-29.md`）。
> 本计划**不含任何新功能**；只含验证、打包、发版说明与观察点。
> 裁决前提：WinUI 暂不交付（W5 不升级门禁）；daemon/后端按 v2 严格语义演进。

## 0. 交付面与版本

- **交付面**：daemon（Rust 二进制）+ webui（静态资源）。WinUI 不在本仓且暂不交付；
  TUI 并入未执行（`crates/qaqh-tui` 不存在）。
- **版本号（决策项，需拍板）**：当前 `2.0.0-alpha4`。建议按 semver 预发布发
  **`2.0.0-beta.1`**（workspace 各 Cargo.toml + webui package.json 同步 bump）。
  若内测口径维持 alpha 号亦可，但发版说明须注明 beta 交付。
- **tag**：建议 `v2.0.0-beta.1`，打在版本 bump 提交上。

## 1. 发版步骤（顺序执行）

### 1.1 G5 补跑 —— 唯一实质验证（先决条件）

本周期从未完整跑过的回归，磁盘预算批准后执行：

- [ ] `cargo test --workspace`（含 daemon 集成测试）
- [ ] webui `bun test` + `tsc --noEmit`（已绿，复跑确认）

**判定规则**：
- 全绿 → 进 1.2；
- 有红 → 逐条定性，三选一：**修**（新回归必须修）、**守卫**（环境性，如
  cancel_keeps_tool_results 的 `sh` skip 先例）、**记录**（存量已知问题，写进
  发版说明第 4 节清单）。禁止带着"无人认领的红"发版。

### 1.2 端到端冒烟复跑

- [ ] `scripts/smoke-g1.ps1`（真实数据根全流程，可重复执行）。

理由：本周期动过 v2 信封字段（ts_ms）与 timeline 响应（归档页 turn_index），
虽有单测与回归锁，发版前用真实 daemon+gateway+浏览器路径再踩一遍。

### 1.3 提交、版本与打包

- [x] 工作分批提交完毕（`0607efa`…`ea33521`，工作区 clean）。
- [ ] 版本号 bump（见第 0 节决策项）。
- [ ] 打 tag + 构建产物：daemon 二进制 + webui 静态资源。
- [ ] 构建产物冒烟：产物形态（非 `cargo run`）再跑一遍 smoke-g1。

### 1.4 发版说明必须携带

- **已知存量问题**（handoff 第四节）：cancel 测试无 `sh` 环境跳过；
  qaqh-sandbox / qaqh-mcp unused 警告；legacy draft 已归档勘误。
- **v2 events 流 kind 可达性说明**：`turn_finished` / `assistant_block_sealed`
  等 kind 的 fact 产生侧缺失，当前实际不可达（已挂 P4）；webui 设计上把事件当
  刷新信号、数据走 RPC/timeline 快照，功能不受影响。
- **设计行为**：webui 无从零建会话流（网关命令要求 active seed，白名单仅
  SendMessage/Cancel）；网关对 session_delete 返回 403 `command_not_allowed`。
- **磁盘兼容**：canonical log 只**新增** `CompactionApplied` fact 类型（W3），
  旧会话数据无该 fact = 前端无压缩分隔，属正常降级，无迁移。

### 1.5 beta 窗口观察点（发给内测用户的重点验证面）

- [ ] B9 断线重连：断网/休眠恢复后自动补发（events cursor / timeline lastEventId）。
- [ ] timeline 翻页：长会话触顶懒加载、页淘汰后触底自动重拉。
- [ ] 压缩分隔：长会话触发 auto-compact 后「此前已压缩」是否出现。
- [ ] SessionCreate 经 commands 通道（webui 发命令路径，BUG-01 修复面）。
- [ ] SSE 稳定性：不再出现 15s 周期断流（BUG-02 修复面）。

## 2. 风险与回滚

- **唯一未验证面** = 1.1 的 workspace 全量回归。翻红处置规则已定（见上），
  量级预期为修复级而非结构级（主要路径已被门禁与冒烟踩过两轮）。
- 发版动作全部可逆：tag 可删、版本号可回；不涉数据迁移与格式变更。
- 发布后如需热修：走 beta 点版本（`2.0.0-beta.2`），禁止把 P 类重构混入热修。

## 3. Beta 后第一件事（不进本窗口）

- P1 file-mutation-delta Step 1（唯一涉 wire 的三件套，宜最先）评审立项。
- P4 增补项：TurnStarted/TurnFinished/AssistantBlock fact 产生侧是否补齐，
  与 P2 context-ownership 协同裁决（handoff 第三节已挂）。

## 4. 一页 checklist（打钩即发）

- [ ] 1.1 workspace 全量 + daemon 集成：绿，或每条红已定性
- [ ] 1.2 smoke-g1 复跑通过
- [ ] 1.3 版本号 / tag / 构建产物 / 产物冒烟
- [ ] 1.4 发版说明（含已知问题清单）
- [ ] 1.5 观察点清单随包分发
