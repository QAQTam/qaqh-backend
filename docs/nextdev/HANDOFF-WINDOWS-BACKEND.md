# 交接文档 —— Windows 侧后端开发团队（最终版）

- **日期**：2026-08-18
- **交付方**：Windows 侧后端开发团队
- **接收方**：后端团队
- **状态**：✅ 本文档为最终定稿；GitHub 已恢复正常；Windows 侧 Gitea 已配置**定时推送**到 GitHub；Linux 侧 Gitea **退役**。

---

## 0. 最终工作模型（决定）

- **Windows 本机 Gitea**（`qaqtam/QAQ-Harness`，`http://localhost:3000`）= **开发 / 评审主源（可写）**
- 已配置 **定时推送 → GitHub**（上游备份 / 发布）
- **权威约定：唯一写入口 = Windows Gitea；GitHub 由定时推送保持同步，避免双写冲突。**
- 仓库性质：**GitHub 迁移副本（非拉取型镜像）** + 手动/定时推送；`mirror=false`，可写。

---

## 1. 远端拓扑（最终）

| 名称 | URL | 用途 | 状态 |
|---|---|---|---|
| `win-gitea`（推荐 origin）| `http://localhost:3000/qaqtam/QAQ-Harness.git` | **主源：开发/评审/推送** | ✅ 在用 |
| `github` | `https://github.com/QAQTam/QAQ-Harness.git` | 上游 / 定时推送目标 | ✅ 在用 |
| `origin`（若仍指 Linux）| `http://60.0.0.32:3000/qaq/QAQ-Harness.git` | 旧 Linux Gitea | ⛔ 已退役，建议改指 `win-gitea` |
| `lan-ssh` | `ssh://qaqtamsy@60.0.0.32:2222/.../qaqh-backend.git` | 旧 SSH 备份 | ⛔ 已退役 |
| `qaqh-backend` | `git://60.0.0.32/qaqh-backend.git` | 旧只读镜像 | ⛔ 已退役（9418 只读勿 push）|

> 建议：`git remote set-url origin http://localhost:3000/qaqtam/QAQ-Harness.git`

---

## 2. 当前分支 / 基线

- `main`：GitHub `c78bb35` == Windows Gitea `c78bb35`（迁移副本一致）；本次修订把 Linux 侧那 3 个 LAN infra 文档提交并入 `main` 并推送到 Windows Gitea（其定时推送会继续同步到 GitHub）。
- `refactor/nextdev-tool-mode-single-contract`：
  - GitHub / Windows Gitea：`7c3b0b3`（WIP 占位）
  - 本地：`10eb649` = `7c3b0b3` + `docs/ReplayMapping.md` + 本交接文档（旧版）
  - 本次一并同步到 Windows Gitea（fast-forward），供后端开 PR review。
- 编译验证：`cargo check --workspace --offline` 在 `8fda6fb`、`7c3b0b3` 两轮 rebase 后均通过。

---

## 3. Issue / PR 现状

- **Windows Gitea（主源，原生迁移）**：
  - Issues：`#9`、`#15`、`#18`（与 GitHub 编号一致）
  - PRs：`#1–8,10–14,16,17,19–22`（18 merged + `#20` open，与 GitHub 一致）
- **Linux Gitea 重放记录（已退役归档）**：22 条重放（`#1–#22`）+ 真实 PR `#23` → 仅存档，编号对照见 `docs/ReplayMapping.md`（归档版）

---

## 4. 备份快照

- 目录：`/tmp/qaqh-gh-backup-2026-08-17/`（**临时目录，建议转存稳定位置**）
- 内容：`items_all.json`、`comments/<n>.json`、`pr-comments/<n>.json`、`reviews/<n>.json`、`events/<n>.json`、`SUMMARY.md`、`ReplayMapping.md`

---

## 5. 已知问题 / 注意事项

- 定时推送间隔内，GitHub 会有**短暂滞后**（取决于配置的间隔）。
- **不要直接在 GitHub 写**（直接 push 会与定时推送冲突/non-fast-forward），除非明确单写入口临时改为 GitHub。
- Gitea 走明文 HTTP：局域网内可用；对外建议加 HTTPS 反代。
- `git://`（9418）只读：**不要对它 push**，已退役。
- GitHub 侧历史上的 `#18`（review issue）在 GitHub 可用时可直接查看；Windows Gitea 上对应迁移后的同号 issue。

---

## 6. 交接 / 待办

- [ ] 在 **Windows Gitea** 上为 `refactor/nextdev-tool-mode-single-contract` 开 PR 供后端 review（对 main）
- [ ] 确认/调整 Windows Gitea 定时推送间隔与目标
- [ ] 把快照目录从 `/tmp` 转存到稳定位置
- [ ] 清理本地已退役 remote：`lan-ssh`、`qaqh-backend`、指向 Linux 的 `origin`（或直接改指 `win-gitea`）
- [ ] Linux Gitea 服务 / 9418 daemon 正式下线（数据先按上面归档）

---

## 7. 常用命令

```bash
# 主源（Windows Gitea）
git fetch win-gitea
git push win-gitea main
git push win-gitea refactor/nextdev-tool-mode-single-contract

# 定时推送目标（GitHub）——正常由 Windows Gitea 定时推送完成，人工一般不用动
git fetch github
git push github main

# 本地 origin 改指主源
git remote set-url origin http://localhost:3000/qaqtam/QAQ-Harness.git

# 观测
#   http://localhost:3000/qaqtam/QAQ-Harness
#   http://localhost:3000/qaqtam/QAQ-Harness/pulls
```
