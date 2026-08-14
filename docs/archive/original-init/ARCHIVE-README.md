# 原始初始提交文档归档（135a558 / 2026-07-26）

> 状态：**归档**（只读快照，永久保留）
> 范围：`135a558`（2026-07-26 11:41:46 +0800 "Initial commit"，root commit）中的全部文档文件
> 来源：`git archive 135a558` 字节级导出（2026-08-15）
> 链接：root commit `135a558`（无 parent）→ 当前 main `d9a2cd2`；相关考古见本目录 `MIGRATION.md` 与根 `README.md`

## 结论

本目录是 DeepX 技术栈演进的**起点快照**，用于警示与考古：

1. DeepX 起步于 **Electron + SolidJS 前端（0.9.0）**，且在此之前曾实现过一个
   **Tauri 对照客户端**，Electron 达到功能对等后即废弃（见
   `apps/desktop/MIGRATION.md`）。技术栈完整演进：Tauri（废弃）→
   Electron（弃用）→ WinUI3 原生（当前，beta.8 起）。
2. 初始仓库由三个独立仓库归一化而成：`D:\DeepX`（后端）、`D:\deepx-desktop`
   （前端）、`D:\DeepXInstaller`（安装器）——因此 git 历史从 `135a558` 重新
   起算，更早的仓库历史不在本仓库对象库内。
3. 初始 workspace 含现已删除的 crate：`deepx-tools`、`deepx-vector`、
   `deepx-companion`、`ratatui-markdown`（本目录保留了 `deepx-vector` 与
   `ratatui-markdown` 的设计/计划文档）。

## 文件清单（与 135a558 原始路径对应）

| 归档路径 | 原始路径（135a558） | 说明 |
|---|---|---|
| `README.md` | `README.md` | 最初版根 README（版本 0.9.0，Electron 时代） |
| `version.txt` | `version.txt` | 初始版本号 `0.9.0` |
| `apps/desktop/MIGRATION.md` | `apps/desktop/MIGRATION.md` | 桌面端迁移记录（含 **Tauri 对照实现废弃** 的事实） |
| `apps/desktop/README.md` | `apps/desktop/README.md` | Electron 前端说明 |
| `apps/installer/README.md` | `apps/installer/README.md` | 原生安装器说明 |
| `crates/deepx-config/prompts/backend_prompt.md` | 同左 | 初始系统提示词（已随时代演进） |
| `crates/deepx-config/prompts/backend_prompt - 副本.md` | 同左 | 初始提交中的冗余副本（如实保留） |
| `crates/deepx-config/prompts/os_env.md` | 同左 | 初始 OS 环境提示词 |
| `crates/deepx-skills/DESIGN.md` | 同左 | Skills 设计文档 |
| `crates/deepx-vector/DESIGN.md` | 同左 | 已删除 crate `deepx-vector` 的设计文档 |
| `crates/ratatui-markdown/PLAN.md` | 同左 | 已删除 crate `ratatui-markdown` 的计划 |
| `crates/ratatui-markdown/README.md` | 同左 | 已删除 crate 的说明 |

## 用途与维护约定

- **只读**：本目录内容与 `135a558` 逐字节对应，不随代码演进修改。
- 需要参考"当前"文档请读 `docs/nextdev/` 与根 `README.md`。
- 新增归档文件需在本文件文件清单中登记，并追加变更日志。

## 变更日志

- 2026-08-15：创建归档（`git archive 135a558` 导出 12 个文件 + 本说明）。
