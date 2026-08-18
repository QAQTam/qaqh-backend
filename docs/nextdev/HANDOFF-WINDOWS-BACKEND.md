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
| `origin` | `http://localhost:3000/qaqtam/QAQ-Harness.git` | **唯一 remote：主源（开发 / 评审 / 推送）** | ✅ 在用 |

> - **本仓库只保留这一个 remote**。`github`、Linux `qaq`、`lan-ssh`、`qaqh-backend` 均已删除，杜绝误推 GitHub 导致分叉。
> - GitHub 仅作为 **Windows Gitea 侧的定时推送目标**（配置在 Gitea 里，不占本地 remote）。

---

## 2. 当前分支 / 基线

- `main`：Windows Gitea `7f6cf00` == 本地 `main`（与 `origin` 对齐）。基线以 GitHub `c78bb35` 出发，并入 Linux 侧那 3 个 LAN infra 文档提交 + 本交接文档；GitHub 由 Windows Gitea 定时推送同步至 `7f6cf00`。
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
- [x] 清理本地 remote：已收敛为仅 `origin`（Windows Gitea）；`github` / `lan-ssh` / `qaqh-backend` / Linux `origin` 均已删除
- [ ] Linux Gitea 服务 / 9418 daemon 正式下线（数据先按上面归档）

---

## 7. 常用命令

```bash
# 唯一 remote：origin（Windows 本机 Gitea）
git fetch origin
git push origin main
git push origin refactor/nextdev-tool-mode-single-contract

# GitHub 由 Windows Gitea 定时推送自动同步（本地不需要也禁止配 github remote，防止误推）

# 观测
#   http://localhost:3000/qaqtam/QAQ-Harness
#   http://localhost:3000/qaqtam/QAQ-Harness/pulls
```
