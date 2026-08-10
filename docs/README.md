# DeepX 文档索引

> 供人 & LLM 会话快速定位文档。**新会话建议先读「必读」分组**。
> 归类原则：按主题分组，物理文件位置不变（保留 git 历史与链接）；过时文档明确标注，不删除。

## 必读（新会话起点）

| 文档 | 内容 | 状态 |
|---|---|---|
| [reactor-winui3-knowledge.md](./reactor-winui3-knowledge.md) | **Reactor/WinUI3 实战知识库**：当前基线、致命陷阱（component 修饰符/Empty 过滤/Grid 隐式 Auto/Wrap）、chat 高频路径、排查方法论、打包版本标识 | ✅ 现行（2026-08-10 沉淀） |
| [windows-reactor-skill.md](./windows-reactor-skill.md) | reactor 心智模型、最小骨架、错误模型（补丁清单） | ⚠️ 主体有效，头部基线引用已过时（见知识库 §1） |
| [winui-native-migration.md](./winui-native-migration.md) | WebView/Electron 移除收官：全原生视图族架构 | ✅ 现行 |
| [winui-native-architecture-audit.md](./winui-native-architecture-audit.md) | 原生壳架构审计（Ringing 基线） | ✅ 现行 |
| [winui-chat-rendering-maintenance.md](./winui-chat-rendering-maintenance.md) | ChatView 渲染维护契约 | ✅ 现行 |
| [PERF-FIX-NOTES.md](../apps/winui/PERF-FIX-NOTES.md) | ChatView 性能修复记录（2026-08，多树解绑） | ✅ 现行 |
| [winui-fluent-visual-system.md](./winui-fluent-visual-system.md) | Fluent 视觉系统（ThemeRef 令牌/间距/字体） | ✅ 现行 |

## 原生 UI 设计与计划（winui）

| 文档 | 内容 | 状态 |
|---|---|---|
| [winui-gallery-porting.md](./winui-gallery-porting.md) | WinUI Gallery / Community Toolkit 视觉移植 | ✅ 现行 |
| [PLAN-NATIVE-CHATVIEW.md](../apps/winui/PLAN-NATIVE-CHATVIEW.md) | ChatView 迁移计划（分层路线） | ✅ 已执行（对照现状阅读） |
| [PLAN-NATIVE-COMPOSER.md](../apps/winui/PLAN-NATIVE-COMPOSER.md) | Composer 原生化计划 | ✅ 已执行 |
| [PLAN-NATIVE-STATEMACHINE.md](../apps/winui/PLAN-NATIVE-STATEMACHINE.md) | 状态机迁移计划（Web → Rust 直连） | ✅ 已执行 |
| [REPORT-NATIVE-STATE-ARCHITECTURE.md](../apps/winui/REPORT-NATIVE-STATE-ARCHITECTURE.md) | 壳数据通道架构调查报告 | ✅ 现行 |
| [UPSTREAM-CAPABILITY-REQUEST.md](../apps/winui/UPSTREAM-CAPABILITY-REQUEST.md) | windows-rs fork 上游能力需求单 | ✅ 现行 |
| [README.md](../apps/winui/README.md) | winui 壳项目说明 | ✅ 现行 |

## 历史 / 参考（Web 时代残留，仅归档）

| 文档 | 内容 | 状态 |
|---|---|---|
| [frontend-PLAN.md](./frontend-PLAN.md) | **SolidJS v2 迁移计划** | ❌ 已死（WebView/renderer 整体移除，83ca250） |
| [CHATVIEW-RENDERING-REFERENCE.md](../apps/winui/CHATVIEW-RENDERING-REFERENCE.md) | ChatView 渲染参考（**Web 端**规格） | 🗄 历史参考（原生实现已在 chat_view.rs） |
| [ELECTRON-MIGRATION.md](../apps/winui/ELECTRON-MIGRATION.md) | Electron → WinUI3 迁移盘点 | 🗄 收官存档 |

## 协议与后端（Ringing / daemon）

| 文档 | 内容 | 状态 |
|---|---|---|
| [ringing-command-query-contract.md](./ringing-command-query-contract.md) | Ringing v1 命令/查询契约 | ✅ 现行 |
| [ringing-migration-map.md](./ringing-migration-map.md) | Legacy → Ringing 迁移映射 | ✅ 现行 |
| [rc4-native-client-contract.md](./rc4-native-client-contract.md) | RC4 原生客户端协议冻结与维护手册 | ✅ 现行（rc.4 主线） |
| [timeline-protocol-design.md](./timeline-protocol-design.md) | Native timeline 协议 | ✅ 现行 |
| [responses-api-support.md](./responses-api-support.md) | Responses API 支持矩阵 | ✅ 现行 |
| [PLAN.md](./PLAN.md) | Ringing 双协议迁移计划 | ✅ 已执行 |
| [backend-dataflow/](./backend-dataflow/README.md) | 后端数据流（README/协议锚/优化/收敛计划） | ✅ 现行 |

## 工具协议与根因报告

| 文档 | 内容 | 状态 |
|---|---|---|
| [plan-edit-preview-commit.md](./plan-edit-preview-commit.md) | 编辑工具两阶段协议（预览→提交） | ✅ 现行 |
| [plan-virtual-scroll-paging.md](./plan-virtual-scroll-paging.md) | Chat 分页虚拟滚动方案 | ✅ 已执行（chat_view 窗口化） |
| [root-cause-skill-envelope-injection.md](./root-cause-skill-envelope-injection.md) | 根因分析报告（skill envelope 注入） | ✅ 存档 |

## 文档维护约定

- 新增/更新文档时同步本索引（状态列）。
- 过时文档**不删除**，标 ❌/🗄 保留历史；物理移动需同步所有引用链接。
- 若后续文档数量继续膨胀，可按子目录物理归类（如 `docs/winui/`、`docs/protocol/`、`docs/plans/`）——需一次性迁移并更新引用。
