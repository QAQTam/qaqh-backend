# docs/archive

已完成任务的文档归档。仍在执行或待办的计划/规格不放在这里：
当前施工单是 `docs/spec-architecture-convergence.md`，活跃计划见 `docs/plan-beta-release.md`、
`docs/plan-legacy-protocol-cleanup.md` 等 `docs/plan-*.md` / `docs/spec-*.md`。

| 文件 | 状态 | 说明 |
|---|---|---|
| `legacy-compat-cleanup-draft.md` | 已完成（2026-09-27/29） | 协议硬切 A-E 主体 + H2-H9 + G 节勘误均落地；剩余条目 I1/I3/I4/I5/I6 已挂入 `plan-beta-readiness.md`（W4/W5/W6/P5），本文件仅作历史留档 |
| `plan-beta-readiness.md` | 已完成（2026-09-29 立项） | G1-G5 门禁 + W1-W6 全部完结；beta 已发布（现版本 2.0.0-beta.2） |
| `beta-readiness-2026-09-29.md` | 已完成 | beta-readiness 的实施交接记录 |
| `plan-beta-release.md` 依赖它 | — | 活跃计划 `docs/plan-beta-release.md` 的前提记录，故路径已在彼处更新 |
| `plan-webui-tauri.md` | 已执行（2026-10-02） | Tauri 2 桌面壳 A/B/C/D 代码阶段全部落地 |
| `plan-permission-extraction-v2.md` | 已被实施吸收 | `qaqh-permission` 已拆出（`d9b571d`），提案不再独立存在 |
| `handoff-permission-three-tiers.md` | 已完成 | 权限三级改造完成；`docs/current` 悬空引用豁免记录在其 :107 |
| `bug-ringing-v2-commands-stuck-in-running.md` | 已修复（2026-10-05） | fact 因果链重接；回归记录仍被 `pending_store.rs:598` 等代码注释引用 |
| `audit-legacy-protocol-2026-10-04.md` | 行动项走完 | 审计结论被 P1.2/P1.3 handoff 吸收；`v2-legacy-compat-probe.sh` 口径出处（路径已更新） |
| `legacy-protocol-p1-2026-10-05.md` | 已落地 main | P0 死面 + P1.1（MCP/LSP typed 化）+ P0 尾巴 ④ |
| `legacy-protocol-p04-p13-2026-10-05.md` | 已落地 main | ④、P1.3、P1.2 第一批（10 变体 + 3 访问器） |
| `legacy-protocol-post-merge-2026-10-06.md` | 已合并 | 本线 12 提交随 PR #10 进 `main@82b3755`；仍是 P0/P1/P2 台账之一（见施工单） |
| `tool-sdk-v2-2026-10-06.md` | 已完成 | Tool SDK v2 P1 迁移 + P2 crate 拆分交接；「风险提示」节为独有教训记录 |
| `research-tool-system-modernization-2026-10-06.md` | 已完成 | P2+P3 全部落地（2026-10-06 晚实施记录）；工具重组唯一蓝图与实施记录 |
| `webui-render-perf-2026-10-03.md` | 已落地 | 交付物已入库（`webui/tests/pagination.test.ts`/`equal.test.ts`、`src/session/*`）；其 ts-rs 落点清单已被实际 `just ts-export` 接线取代；报告全文在 `webui/REPORT-render-perf-2026-10-03.md` |
| `windows-sandbox-write-interception-v1.md` | 历史快照 | 自述：权威版本已迁 `E:\win-sandbox-rs\docs\spec\`（ADR 0001），本文件不再更新；spike 已落地为 `crates/sbx-win` |
| `windows-sandbox-cross-review.md` | 历史快照 | 同上；v1 spec 的交叉评审记录（Sandboxie/Codex/AGT 四方对照） |
