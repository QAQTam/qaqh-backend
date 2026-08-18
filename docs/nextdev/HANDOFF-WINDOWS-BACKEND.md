# 交接文档 —— Windows 侧后端开发团队

- **日期**：2026-08-18
- **交付方**：Windows 侧后端开发团队
- **接收方**：后端团队（LAN / Gitea 环境 continue）
- **背景**：GitHub 服务在部分网络不可达 / 维护中，为保证 nextdev tool-mode single-contract 工作可继续推进，临时切到局域网 git 传输 + Gitea 观测/评审，并将 GitHub 的 PR / Issue **快照 + 重放**到 Gitea。

---

## 1. 远端拓扑（Windows 侧本地 remotes）

| 名称 | URL | 用途 | 备注 |
|---|---|---|---|
| `origin` | `http://60.0.0.32:3000/qaq/QAQ-Harness.git` | **主推拉入口（Gitea）** | 仓库已改公开；凭据在 GCM（`60.0.0.32:3000` 通用凭据，用户 `qaq`，admin） |
| `lan-ssh` | `ssh://qaqtamsy@60.0.0.32:2222/home/qaqtamsy/git/qaqh-backend.git` | SSH 可写备份 / 备援传输 | push 可用（实测建删分支成功） |
| `qaqh-backend` | `git://60.0.0.32/qaqh-backend.git` | 只读镜像 | **push 会卡死**（git-daemon 只读，9418），只用于读 |
| ~~origin 原值~~ | `https://github.com/QAQTam/QAQ-Harness.git` | GitHub 恢复后还原 | `git remote set-url origin https://github.com/QAQTam/QAQ-Harness.git` |

> 关键经验：`git://` 只读、push 挂起（receive-pack 未被服务）；写操作走 **SSH** 或 **Gitea HTTP**。

---

## 2. 当前工作分支 / 提交基线

- 分支：`refactor/nextdev-tool-mode-single-contract`
- tip：`cbd10ca`（`docs: add GitHub->Gitea replay mapping`）
- 上一提交：`7c3b0b3`（`feat(backend): WIP nextdev tool-mode single-contract integration`，占位）
- 相对 Gitea `main`（`86812b9`）：**2 领先 / 2 落后**（落后的是 #19/#21 之前的两次 rebase 基线与本轮 Gitea 上新增的 LAN infra 文档）
- 历史说明：原 `35f7f30` 因内容已在 GitHub PR #3 合入而被自动跳过；当前 WIP 是重放后的 70 文件改动
- 编译验证：`cargo check --workspace --offline` 在 `8fda6fb`、`7c3b0b3` 两轮 rebase 后均**通过**

---

## 3. Gitea 上的内容（Windows 侧已重建/重放）

- 仓库：http://60.0.0.32:3000/qaq/QAQ-Harness （公开）
- **原 GitHub Issue** → Gitea Issue：`#9 → #1`、`#15 → #2`、`#18 → #3`
- **原 GitHub 19 个历史 PR** → Gitea Issue（记录型，正文含状态/合并时间/分支/来源 + 原正文 + 评论/review/行内评审）：`#4 ~ #22`
- **真实 open PR**：Gitea **#23**（`refactor/nextdev-tool-mode-single-contract → main`）
- 编号对照表：`docs/ReplayMapping.md`（含全部 GitHub# ↔ Gitea# 映射）
- Gitea 侧 issue/PR 的标题均带 `[#原编号]` 便于检索

---

## 4. 备份快照（重要，注意保留）

- 目录：`/tmp/qaqh-gh-backup-2026-08-17/`（**临时目录**，建议转存稳定位置）
- 内容：
  - `items_all.json`（22 项主清单）
  - `comments/<n>.json`、`pr-comments/<n>.json`、`reviews/<n>.json`、`events/<n>.json`
  - `SUMMARY.md`（人可读全量摘要，728 行）
  - `ReplayMapping.md`（编号映射）

---

## 5. 已知问题 / 注意事项

- `git://60.0.0.32/qaqh-backend.git`（9418）为只读：**不要对它 push**（会卡死）；9418 daemon 是否停用待定。
- Gitea 走明文 HTTP：建议后续加 Caddy/Nginx HTTPS 反代再对外。
- GitHub 与 Gitea 的 `main` 已出现分叉（Gitea 侧新增 LAN infra 等提交）；待 GitHub 恢复后需同步/定基线。
- GitHub 恢复后，若想还原**原生 PR 语义**，可跑正式 Gitea 迁移（会与本次重放重复——需先清理或另建仓库）。
- `cbd10ca`、`7c3b0b3` 均为占位/文档提交，正式合入前建议按计划整理提交与 rebase 基线。

---

## 6. 交接 / 待办

- [ ] 后端 review Gitea **PR #23**（对应 `7c3b0b3` 的 70 文件 WIP）
- [ ] 准备清理并 **rebase 基线**（以 Gitea `main` / 恢复后的 GitHub `main` 为准）
- [ ] 决定 9418 `git://` daemon 去留
- [ ] 为 Gitea 加 HTTPS 反代（可选）
- [ ] 快照目录转存稳定位置
- [ ] GitHub 恢复后同步 main 分叉 + 决定是否正式迁移

---

## 7. 常用命令

```bash
# 拉取 / 推送（当前 origin = Gitea）
git fetch origin
git push origin refactor/nextdev-tool-mode-single-contract

# LAN SSH 备援推送
git push lan-ssh refactor/nextdev-tool-mode-single-contract

# 恢复 GitHub origin（GitHub 恢复后）
git remote set-url origin https://github.com/QAQTam/QAQ-Harness.git

# 浏览器观测 Gitea
#   http://60.0.0.32:3000/qaq/QAQ-Harness
#   http://60.0.0.32:3000/qaq/QAQ-Harness/pulls/23
```
