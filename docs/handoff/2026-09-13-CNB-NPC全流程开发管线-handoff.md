# handoff：CNB NPC 全流程开发管线——issue 派发 → NPC 修复 → reviewer 预审 → 质检 → squash 合并 → 关单（2026-09-13）

> 交接对象：下一个接手 qaqh-backend / qaqh-winui-app 开发循环的 agent 或人。
> 本文是**流程手册 + 当前快照**，不是单次 bug 修复记录。
> 关联：主报告 [`docs/report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md`](../report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md)、buglist [`docs/buglist/2026-09-13-隐藏缺陷全面扫描-buglist.md`](../buglist/)

---

## 1. 一句话交接

39 个已建档 issue 走「CNB NPC 异步修复管线」推进：**当前 20 open / 19 closed**；
wave1（13 修复合入 main）、wave2（7 PR 收到，3 个质检通过已 squash 合并 `d262b8b/e909220/537c098`）已完成；
wave3 已派发 **20 个 issue**（4 返工 + 6 新 P1 + 5 性能 + 5 跟进），NPC 交付 PR 后需按本文 §4 流程收割质检。
**#37 / #38 按约定留本地人工修**，不派 NPC。

> **⚠️ 成本纪律（2026-09-13 深夜起，用户指示，最高优先级）**：NPC 本体免费，但它在云端跑的
> `cargo build/test/clippy` **按量计费（烧真钱）**。此后派发一律「**远端只生成代码，不跑构建/验证**」：
> ① NPC 铁律禁止云端跑 cargo（`.cnb.yml` 已同步改写，main.push 的 rust-ci 已下线）；
> ② 预检/reviewer 模板去掉「实测 cargo test/clippy」项，改为纯静态 diff 审读（本轮 22 条预检每条烧 8~27 分钟构建，是最大的学费）；
> ③ 代码拉回本地 Windows 验证（受影响 crate test + clippy），Windows-only 路径云端本来也测不了（§6.1/6.2）。

## 2. 基础设施与角色（本管线的组成）

| 组件 | 配置 | 说明 |
|---|---|---|
| 仓库 | `https://cnb.cool/QAQ-Harness/qaqh-backend`（CNB，非 GitHub；本地 `D:\project\qaqh-backend`） | 全流程用 CNB：issue / PR / NPC / build / observability，**不要用 gh** |
| NPC `bug-fixer` | 自定义角色，slug `QAQ-Harness/qaqh-backend`，免费 deepseek-v4.1-flash | issue 评论 `@QAQ-Harness/qaqh-backend(bug-fixer)` 触发；**永不传 `model` 参数**（保免费默认），需要时只用 `thinkingLevel`/`maxTurns` |
| NPC `reviewer` | 同组织自定义角色 | PR 上 `@QAQ-Harness/qaqh-backend(reviewer)` 评论触发；已实证能抓真阻断（跨平台越权、内存泄漏型设计），附基线对拍数据 |
| CI（`.cnb.yml`） | Dockerfile Ubuntu 26.04 + Rust 1.98.1 + gcc-15；`cargo clippy --workspace --all-targets -- -D warnings` 门禁 | NPC 云端跑的就是这个环境（Linux），**Windows 特有路径行为云端测不出**（见 §6 教训） |
| 运行环境 | NPC 在云端流水线跑（`npc.work_mode: true` 必须生效，CLI 手动派发漏带会 403 push） | 接单状态看 issue 评论的 `statuses.npc[0].context` + tasks JSON |

## 3. CNB CLI 速查（踩坑后的正确姿势）

```powershell
# argv 直跑（pwsh 偶发吞 stdout → 复杂批量操作写 python 脚本，输出重定向文件再 read）
node C:\Users\QAQTam\AppData\Roaming\npm\node_modules\@cnbcool\cnb-cli\bin\cnb.js <module> <tool> --repo QAQ-Harness/qaqh-backend <args>

# issue 列表（open/closed）
issues list-issues --state open --page-size 100
# 关 issue（⚠️ 假成功陷阱：只传 --state closed 时 rc=0 但状态不变！）
issues update-issue --number N --state closed --state-reason completed
# 发 NPC 派发评论
issues post-issue-comment --number N --body "<派发文案>"
# PR 列表 / 文件（list-pull-files 无分页参数）
pulls list-pulls --state open
pulls list-pull-files --number N --verbose     # verbose 才有完整 patch
# PR 评论（reviewer 意见在这里）
pulls list-pull-comments --number N
# 合并（成功判定认 "merged: true" 子串，别按 JSON 引号格式匹配——已两次误报 FAIL）
pulls merge-pull --number N --merge-style squash --commit-title "<标题> (Closes #N)" --commit-message "<摘要>"
```

**输出格式是 YAML 风格**（`status: 200` / `data[39]:`），不是 JSON。NPC 接单状态在评论的
`statuses: npc[1]: context: pipeline_id/sha` + `tasks:` JSON（每步 in_progress/pending）。

## 4. 标准循环（每个 issue 的完整生命周期）

```
建档 → 派发 → NPC 云端修复 → (reviewer 预审) → PR 交付 → 本地质检 → squash 合并 → 关单
```

1. **建档**：一个 bug 一个 issue，标题 `[BUG-yyyy-mm-dd-nn][P级别] 中文一句话现象`，
   body 含：来源/修复窗口、现象与影响（精确到 `file:line`）、根因、修法、验收标准。
   映射表在 `.qaqh/issue-bodies/issue-map.json`（历史记录）。
2. **派发**：issue 评论固定骨架——`@bug-fixer 请修复本 issue` + 分支名
   `fix/bug-2026-09-13-NN-短名` + PR 标题格式 `fix(scope): 中文描述 (Closes #N)` +
   验证要求（`cargo test --workspace 全绿，exec::tests 2 个既存失败 O-3 除外 + clippy -D warnings`）+
   issue 特有约束。**每次派发前检查 NPC 评论 statuses 确认接单**。
3. **收割**：轮询 `pulls list-pulls --state open`（NPC PR 的 author `is_npc: true`）。
4. **质检（关键，不可省）**：
   a. `pulls list-pull-files --number N --verbose` 审 diff（方案对不对、有没有越界改动）；
   b. `git fetch origin <分支>` → checkout → **本地 Windows** 跑受影响 crate 的
      `cargo test -p <crate>` + `clippy -p <crate> --all-targets -- -D warnings`（云端 Linux 绿 ≠ Windows 绿，见 §6）；
   c. 若 reviewer 已留阻断意见（`list-pull-comments`），不合并，原分支返工派发。
5. **合并**：`merge-pull --merge-style squash --commit-title "<原标题> (Closes #N)"`。
   ⚠️ squash 标题带 `Closes #N` **并不会自动关 issue**（CNB 未实现），需手动
   `update-issue --state closed --state-reason completed` 关闭。
6. **关单**后 `git pull origin main` 同步本地。

**返工循环**：质检/reviewer 阻断 → 在原 PR 分支上派发返工评论（写明每个阻断项的修法）→
NPC push 后重走 §4.4。不要新开分支，保持 PR 关联。

## 5. 当前快照（2026-09-13 19:10）

### main = `537c098`（origin 同步），本地干净

已合并 wave1+wv2（对应 issue 全 closed）：#1-5,9,15,16,17,18,19,20,21,25,26,27 共 17 个。

### wave3 在途（20 个，派发评论已发，等待 NPC PR）

| 组 | Issue | 备注 |
|---|---|---|
| 返工（原分支继续） | #14 → PR#56、#23 → PR#57、#11 → PR#58、#24 → PR#60 | 派发评论里已写明 reviewer/本地质检的每个阻断项与修法 |
| 新 P1 | #6 #7 #8 #10 #12 #13 | WAL fail-closed / drain safe_dispatch / 取消保结果 / 流内吊销 / 空 messages / 悬挂 ToolUse |
| 性能族 | #28 #29 #30 #31 #32 | 各自要求基准对比数据 |
| 跟进族 | #33 #34 #35 #36 #39 | #33 是 BUG-08 锁表收尾；#36 是 O-3 根因排查（允许改测试但要证据） |

### 留本地（不派 NPC）

- **#37**：exec O-3 两个既存失败测试（`exec/tests.rs:201/:254`，中文 args / bash 位置参数）——所有派发文案都写「O-3 除外」，修掉后记得改派发模板。
- **#38**：`just fmt` 收口 91 处，单独 `chore(fmt)` 提交，避免污染 diff。

### 本机已知噪音（非 PR 锅，勿误判）

- `qaqh-session` 8 个 clippy 警告（`empty_line_after_doc_comments`、`redundant_closure`、
  `large_enum_variant`、`field_reassign_with_default` 等，`store/mod.rs:142-150/288`、`manager.rs:252/1265+`）——
  wave1 合并前就存在，`cargo clippy -p qaqh-session --all-targets -- -D warnings` 会红。wave3 收割时若 CI 红优先查这个。
- 本机 `rustc 1.98.1` 的 clippy lint 比云端 CI 抓得更多（新 lint），本机红但云端绿时先比对 lint 名再定性。

## 6. 踩坑记录（勿重复交学费）

1. **Windows verbatim 路径双账本键**（#49 教训）：`canonicalize()` 返回 `\\?\C:\...`，
   与 `resolve_workspace_path` 的普通形态不互认。NPC 云端 Linux 全绿、Windows 才炸。
   → **本地质检必须真跑**，眼见为实。
2. **测试二进制无 `.exe` 后缀**（PR#58）：`cfgset_bin()` 找 `target/debug/examples/cfgset`
   在 Windows 恒 False。NPC 在 Linux 上写测试时天然想不到，质检时优先看测试的文件/路径操作。
3. **clippy 版本差**：本地 1.98.1 的新 lint 会红云端没开 `-D warnings` 的旧 lint；
   判断 PR 是否引入时先 `git blame` 出错行。
4. **CLI 输出判定**：`merged: true` 子串；关单要 `--state-reason completed`；
   `list-pull-files` 没有 `--page-size`，大 PR 用 `--verbose`。
5. **pwsh 直跑 CNB CLI 偶发吞 stdout**：批量操作一律写 python 脚本 + 输出写 `%TEMP%` 文件再 read。
6. **改名遗产**：本地目录已从 `QAQ-Harness` 改名 `qaqh-backend`（winui 侧 7 文件引用已同步，
   PR `9c6e288` 已合入 winui main）；旧文档/issue 里 `D:\project\QAQ-Harness` 字样指同一仓库。
7. **NPC 铁律**（已写进角色 prompt，派发文案里重复一遍作强化）：
   先复现测试（红）→ 修 → `cargo test` + `clippy -D warnings` 全绿 → 分支规范 →
   PR body 四要素（现象/根因/修法/验证记录）→ issue 评论 PR 链接与摘要。
   **（2026-09-13 深夜修订）**：前两项改为「写测试 + 实现但**不跑**（云端 cargo 烧钱，验证本地做）」，
   PR body 的「验证摘要」改为「本地验证清单」——见 §1 成本纪律。

## 7. 收割 wave3 的操作序列（下次开工直接照做）

**推荐：直接用 MCP 监视器**（`tools/cnb-mcp-enhance/`，已入库 a4fe80b）——
官方 [`@cnbcool/mcp-server`](https://cnb.cool/cnb/tools/cnb-mcp-server) 覆盖 50+ 工具（issues/pulls/build CRUD），
本仓增强层补齐 NPC 编排缺口（npc.workMode 触发、npc-observability、wave 聚合监视）：

```powershell
# 独立监视器（不经 MCP 客户端也能用）：一次看全部 issue 的构建/评论/PR 状态
$env:CNB_REPO="QAQ-Harness/qaqh-backend"; node tools/cnb-mcp-enhance/server.mjs --watch --issues 6,7,8,10,12,13

# MCP 客户端注册（.mcp.json）两 server 并存：
# "cnb":         npx -y -p @cnbcool/mcp-server cnb-mcp-stdio   （官方全量）
# "cnb-enhance": node tools/cnb-mcp-enhance/server.mjs          （dispatch/observations/watch/close 5 工具）
```

关键 API 事实（调试时勿踩）：
- CNB_TOKEN 8h 过期，cnb CLI 自动 refresh 并回写 `~/.cnb/token`（JSON 的 access_token 字段）——server 已实现每次请求重读；
- CNB API 缺 `Accept: application/json` 头返 406；
- stage 详情路径是 `/-/build/logs/stage/{sn}/{pipelineId}/{stageId}`；
- 关单必须 `state=closed` + `state_reason=completed` 同时传。

**程序化 NPC 派发（等价 UI 勾「替我上班」）**：`api_trigger_wm` 流水线（.cnb.yml 已定义）——
不传 npc 字段的 api_trigger 拿「可信事件 scope」（repo-code:rw），npc:go 从 env `WAVE3_TASK` 读任务提示词。
CLI 也可直接 `build start-build --npc-name CodeBuddy --npc-workMode`（仅 CodeBuddy 身份）。

原生 CLI 收割序列（MCP 不可用时的退路）：

```powershell
# 1. 看新 PR
node <cli> pulls list-pulls --repo QAQ-Harness/qaqh-backend --state open
# 2. 逐个：diff 审查 + reviewer 意见
node <cli> pulls list-pull-files --repo QAQ-Harness/qaqh-backend --number N --verbose
node <cli> pulls list-pull-comments --repo QAQ-Harness/qaqh-backend --number N
# 3. 本地质检（参照 .qaqh/qc_wave2.py 模式：fetch → checkout → cargo test/clippy → 回 main）
# 4. 通过 → squash 合并 → 手动关 issue（--state-reason completed 不可省）→ git pull
# 5. 阻断 → 原分支派返工（模板见 wave3 派发评论，#14/#23/#11/#24 各有针对性修法）
```

wave3 全部收割完成后：issue 库清零（除 #37/#38 本地遗留），届时评估是否给 tui-app 仓
复制这套管线（tui-app 侧 `ci/npc-env-and-roles` 移植已就绪，等 PR #1 合并）。
