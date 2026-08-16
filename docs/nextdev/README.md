# DeepX 文档索引（nextdev）

> 2026-08-16 重整：`docs/nextdev/` 仅保留**现行有效**文档；其余旧计划/设计文档已 `git mv` 归档至 [../backlog/](../backlog/)（待办/历史，保留 git 历史）。
> 新会话先读：**[REFACTOR-ROADMAP.md](./REFACTOR-ROADMAP.md)**（当前重构方向）+ **[bugfix-1.1-nextdev.md](./bugfix-1.1-nextdev.md)**（Bug 档案）。

## 现行有效（nextdev 根）

| 文档 | 内容 | 状态 |
|---|---|---|
| [REFACTOR-ROADMAP.md](./REFACTOR-ROADMAP.md) | **架构重构路线图**：七刀 + 三阶段（砍架构→统一语义→谈创新）；多前端决策；文件级清单 | ✅ 现行（方向已定稿 2026-08-16） |
| [bugfix-1.1-nextdev.md](./bugfix-1.1-nextdev.md) | **Bugfix 档案**：BUG-001~013，现象/根因/证据 | ✅ 现行（持续追加） |

## 法律与合规（保留）

| 文档 | 内容 | 状态 |
|---|---|---|
| [legal/USER_AGREEMENT.zh-CN.md](./legal/USER_AGREEMENT.zh-CN.md) | 用户协议 | ✅ 现行（公测必备） |
| [legal/PRIVACY_POLICY.zh-CN.md](./legal/PRIVACY_POLICY.zh-CN.md) | 隐私政策 | ✅ 现行（公测必备） |
| [legal/legal-consent.schema.json](./legal/legal-consent.schema.json) | 同意状态 schema | ✅ 现行 |
| [legal/version.txt](./legal/version.txt) | 协议文档版本号 | ✅ 现行 |

## 待办 / 历史（`docs/backlog/`）

> 原 `nextdev/` 下的 PLAN-*、winui-*、协议/子代理/工作流等 31 份旧文档已移入 `docs/backlog/`。
> 这些是重构前的计划/设计/审计，**仅供方向参考，不再视为当前有效计划**。重构时按 [REFACTOR-ROADMAP.md](./REFACTOR-ROADMAP.md) 的刀序回溯可用素材（如 PLAN-WORKSPACE-INPROCESS、TIMELINE-MIGRATION-ROADMAP、PLAN-TOOL-MODES 等）。

## 文档维护约定

- `docs/nextdev/` = 现行有效（重构路线 + bugfix + legal）。
- `docs/backlog/` = 待办/已归档旧计划（git mv 保留历史，不删除）。
- 过时文档**不删除**：移入 `docs/backlog/` 或 `docs/archive/` 并同步本索引。
