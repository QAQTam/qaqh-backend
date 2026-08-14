# DeepX 文档索引（nextdev）

> 供人 & LLM 会话快速定位文档。**新会话建议先读「必读」分组**。
> 归类原则（2026-08-13 重整）：`docs/nextdev/` = 现行有效文档；`docs/archive/` = 已完成/已废弃的历史文档。
> 过时文档不删除，移入 `../archive/` 保留 git 历史。

## 必读（新会话起点）

| 文档 | 内容 | 状态 |
|---|---|---|
| [rc4-native-client-contract.md](./rc4-native-client-contract.md) | **RC4 原生客户端协议冻结与维护手册**（rc.4 主线） | ✅ 现行（rc.4 冻结基线） |
| [reactor-winui3-knowledge.md](./reactor-winui3-knowledge.md) | **Reactor/WinUI3 实战知识库**：当前基线、致命陷阱（component 修饰符/Empty 过滤/Grid 隐式 Auto/Wrap）、chat 高频路径、排查方法论、打包版本标识 | ✅ 现行 |
| [windows-reactor-skill.md](./windows-reactor-skill.md) | reactor 心智模型、最小骨架、错误模型（补丁清单） | ✅ 现行 |
| [winui-chat-rendering-maintenance.md](./winui-chat-rendering-maintenance.md) | ChatView 渲染维护契约 | ✅ 现行 |
| [winui-fluent-visual-system.md](./winui-fluent-visual-system.md) | Fluent 视觉系统（ThemeRef 令牌/间距/字体） | ✅ 现行 |
| [timeline-protocol-design.md](./timeline-protocol-design.md) | Native timeline 协议 | ✅ 现行 |
| [PERF-FIX-NOTES.md](../apps/winui/PERF-FIX-NOTES.md) | ChatView 性能修复记录 | ✅ 现行 |

## 原生 UI 设计与计划（winui）

| 文档 | 内容 | 状态 |
|---|---|---|
| [CHATVIEW-V4-PLAN.md](./CHATVIEW-V4-PLAN.md) | ChatView V4 改进计划（ToolView 聚合 + 状态设计） | ✅ 原型已定稿，待 Rust 落地 |
| [PLAN-SUBAGENT-STATUS-AREA.md](./PLAN-SUBAGENT-STATUS-AREA.md) | 并行子代理 UI（工作状态区）实施计划 | ✅ 进行中（MVP 已落地） |
| [PLAN-WORKSPACE-CAPABILITY.md](./PLAN-WORKSPACE-CAPABILITY.md) | Workspace 升级：Capability 抽象 + MCP + Hooks 权限模型 | 🔲 待实施（P1 已落地） |
| [PLAN-WINUI-WORKSPACE-NAV.md](./PLAN-WINUI-WORKSPACE-NAV.md) | WinUI 侧栏工作区化：Workspace 导航 + 标签页过滤 + 未分组孤儿语义 | 🔲 待实施（2026-08-14 定稿） |
| [chat-live-prototype.html](./chat-live-prototype.html) | ChatView V4 交互原型（定稿） | ✅ 落地依据 |
| [chat-pagination-design.md](./chat-pagination-design.md) | ChatView 历史分页：机制实测 + 设计细化（阶段 3 后端分页待实施） | ✅ 现行（待 rc4 冻结决策） |
| [plan-virtual-scroll-paging.md](./plan-virtual-scroll-paging.md) | Chat 分页虚拟滚动方案（阶段 1-2 已完成） | ✅ 进行中（阶段 3） |
| [winui-gallery-porting.md](./winui-gallery-porting.md) | WinUI Gallery / Community Toolkit 视觉移植 | ✅ 现行 |
| [winui-update-design.md](./winui-update-design.md) | WinUI 更新：测试阶段闭环（package/update/ 静态源 + 基线增量 + 弹窗 UX） | ✅ 现行 |
| [WINUI-NATIVE-CONTROL-DESIGN-AUDIT.md](./WINUI-NATIVE-CONTROL-DESIGN-AUDIT.md) | 原生控件复用与设计审查（设计基线） | ✅ 现行 |
| [WINUI-WINDOWS-NOTIFICATION-INTERACTION-REPORT.md](./WINUI-WINDOWS-NOTIFICATION-INTERACTION-REPORT.md) | Windows 交互式通知：可行性报告与实现方案 | ✅ 现行（待实施） |
| [PLAN-NATIVE-CHATVIEW.md](../apps/winui/PLAN-NATIVE-CHATVIEW.md) | ChatView 迁移计划（分层路线） | ✅ 已执行（对照现状阅读） |
| [PLAN-NATIVE-COMPOSER.md](../apps/winui/PLAN-NATIVE-COMPOSER.md) | Composer 原生化计划 | ✅ 已执行 |
| [PLAN-NATIVE-STATEMACHINE.md](../apps/winui/PLAN-NATIVE-STATEMACHINE.md) | 状态机迁移计划（Web → Rust 直连） | ✅ 已执行 |
| [REPORT-NATIVE-STATE-ARCHITECTURE.md](../apps/winui/REPORT-NATIVE-STATE-ARCHITECTURE.md) | 壳数据通道架构调查报告 | ✅ 现行 |
| [UPSTREAM-CAPABILITY-REQUEST.md](../apps/winui/UPSTREAM-CAPABILITY-REQUEST.md) | windows-rs fork 上游能力需求单 | ✅ 现行 |
| [README.md](../apps/winui/README.md) | winui 壳项目说明 | ✅ 现行 |

## 上游适配与迁移（windows-reactor）

| 文档 | 内容 | 状态 |
|---|---|---|
| [WINDOWS-REACTOR-UPSTREAM-ADAPTATION.md](./WINDOWS-REACTOR-UPSTREAM-ADAPTATION.md) | 上游 microsoft/windows-rs 合并适配记录（破坏性变更 + 遗留事项） | ✅ 现行（改 apps/winui 或 fork 前必读） |
| [TIMELINE-MIGRATION-ROADMAP.md](./TIMELINE-MIGRATION-ROADMAP.md) | Timeline 迁移路线图（rc3 → rc5，Phase 1 已完成） | ✅ 进行中 |

## 协议与后端（Ringing / daemon）

| 文档 | 内容 | 状态 |
|---|---|---|
| [rc4-native-client-contract.md](./rc4-native-client-contract.md) | RC4 原生客户端协议冻结与维护手册 | ✅ 现行（rc.4 主线） |
| [timeline-protocol-design.md](./timeline-protocol-design.md) | Native timeline 协议 | ✅ 现行 |
| [responses-api-support.md](./responses-api-support.md) | Responses API 支持矩阵 | ✅ 现行 |
| [doc/future-optimizations.md](./doc/future-optimizations.md) | daemon 未来优化方向（P2+） | ✅ 参考 |

## 子代理（subagent）

| 文档 | 内容 | 状态 |
|---|---|---|
| [subagent-exec-sandbox.md](./subagent-exec-sandbox.md) | B2：子代理 exec 沙箱化设计（弱沙箱 → AppContainer/Landlock 分层） | 📋 设计稿（B1 已落地：权限沙箱自动批准/拒绝） |

## 外部对标（deepseek-harness）

| 文档 | 内容 | 状态 |
|---|---|---|
| [dsh-alignment.md](./dsh-alignment.md) | dsh（@deepseek-ai/dsh）调研：技术栈/模型增益机制/分级对齐清单（A 随发版、B 第一批、C 排期）/co-work-subagent 前瞻 | ✅ 清单定稿（dsh 8/13 开源） |

## 法律与合规

| 文档 | 内容 | 状态 |
|---|---|---|
| [legal/USER_AGREEMENT.zh-CN.md](./legal/USER_AGREEMENT.zh-CN.md) | 用户协议 | ✅ 现行（公测必备） |
| [legal/PRIVACY_POLICY.zh-CN.md](./legal/PRIVACY_POLICY.zh-CN.md) | 隐私政策 | ✅ 现行（公测必备） |
| [legal/legal-consent.schema.json](./legal/legal-consent.schema.json) | 同意状态 schema | ✅ 现行 |
| [legal/version.txt](./legal/version.txt) | 协议文档版本号 | ✅ 现行 |
| [PRIVACY-AUDIT-PRELAUNCH.md](./PRIVACY-AUDIT-PRELAUNCH.md) | 上线前隐私专项审计 | ✅ 现行（公测前必读） |

## 历史 / 归档（`../archive/`）

已完成或已废弃的文档，仅作历史参照：

- [PLAN.md](../archive/PLAN.md) — Ringing 双协议迁移计划（已执行）
- [frontend-PLAN.md](../archive/frontend-PLAN.md) — SolidJS v2 迁移计划（WebView 已移除）
- [ringing-migration-map.md](../archive/ringing-migration-map.md) — Legacy → Ringing 迁移映射（迁移已收官）
- [ringing-command-query-contract.md](../archive/ringing-command-query-contract.md) — 改造期命令/查询契约（被 rc4 契约取代）
- [winui-native-migration.md](../archive/winui-native-migration.md) — WebView/Electron 移除收官记录
- [winui-native-architecture-audit.md](../archive/winui-native-architecture-audit.md) — 原生壳架构审计
- [winui-native-ui-design-audit.md](../archive/winui-native-ui-design-audit.md) — 原生 UI 设计审计盘点
- [plan-edit-preview-commit.md](../archive/plan-edit-preview-commit.md) — 编辑工具两阶段协议（已落地）
- [root-cause-skill-envelope-injection.md](../archive/root-cause-skill-envelope-injection.md) — skill envelope 注入根因报告
- [backend-dataflow/](../archive/backend-dataflow/README.md) — Electron/SolidJS 时代数据流契约与优化计划
- [debug.md](../archive/debug.md) — ASK 弹窗链路排查交接（已修复）
- [diff-link-demo.txt](../archive/diff-link-demo.txt) — diff 链接 demo

## 文档维护约定

- 新增/更新文档时同步本索引。
- 过时文档**不删除**：从 `nextdev/` 移入 `../archive/`（git mv 保留历史），并更新本索引。
- 物理移动需同步所有引用链接。
