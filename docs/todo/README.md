# todo — 报告与可执行 checklist（v1，2026-09-17 立）

> 本目录是**公共文档**：一次复核/审查产出一份 report，report 里拆出的待办立刻落成
> 一份 **checklist**（GitHub issue 风格）。之后派 codex-cli 子代理时，
> **按 checklist 逐条下发**，不再重新解释背景。

## 1. 为什么要有这一层

`docs/report/` 只负责**证据与结论**（见 `docs/report/TEMPLATE.md`），`docs/plan/` 负责设计。
两者都不适合直接当执行输入：report 太长、plan 太抽象。本目录补的是**可执行粒度**——
每条待办自带位置、证据、期望动作、验收命令，子代理不需要上下文就能独立跑完并自证。

## 2. 命名与位置

- 目录：`docs/todo/`
- 报告：`{yyyy-mm-dd}-{标题}-report.md`（沿用 `docs/report/TEMPLATE.md` 的必备章节）
- 清单：`{yyyy-mm-dd}-{标题}-checklist.md`
- 一次复核一对文件，日期=成稿日；同一主题的续作用新日期新文件，并在「关联」互引

## 3. checklist 格式（固定）

```markdown
# {标题} — 执行清单（{yyyy-mm-dd}）

> 来源：`./{yyyy-mm-dd}-{标题}-report.md`
> 基线：{commit}；验收一律以「命令 + 期望输出」表述
> 用法：**一个批次 = 一次 codex-cli 下发**；批次内条目按序执行，跨批次互不依赖

## 批次 N：{批次名}  ← 可独立下发
- 依赖：无 / 批次 M
- 涉及：`path/a.rs`、`path/b.rs`

- [ ] **T-{批次}-{序} {一句话标题}**（严重度 P0/P1/P2/P3）
  - 位置：`path/a.rs:123`
  - 现状：{为什么现在是坏的，带证据等级 E1/E2/E3}
  - 动作：{最小改动；有多个方案时写清取舍}
  - 验收：`{可复制执行的命令}` → 期望 `{输出}`
  - 关联：BUG-xxx / D-x
```

规则：

1. **一条待办 = 一次可独立验收的改动**。改不动的（需现场复测、需外部仓库）不写进批次，
   统一放最后的「阻塞项」小节并注明缺什么。
2. 每条必须能回答三个问题：**改哪里、改成什么、怎么证明改好了**。答不全的退回 report 补证据。
3. 复选框只由执行者勾选；`[x]` 之后必须补 `→ {commit}` 或 `→ 验收命令原文`，禁止空勾。
4. 严重度口径与 `docs/report/TEMPLATE.md` §3 一致。

## 4. 派 codex-cli 的固定做法

**二进制只用** `/home/qaqtamsy/Desktop/ws/codex`（其余路径下的是旧版实现）。
**注意**：它依赖 `node` 在 PATH 中；在非登录 shell（如 git credential helper）里必须用绝对路径。

### 4.1 ⚠️ CLI 子代理不是真子代理，但沙箱是真的（已实测）

我们走的是 CLI 伪子代理，**不是** codex 的原生子代理机制。不过 `-s` 沙箱在**挂载层**生效，
实测边界如下（2026-09-17 用 4 组探针验证；shell 与文件编辑工具同源受限）：

| 目标路径 | `-s workspace-write -C <worktree>` | `-s read-only` |
|---|---|---|
| `-C` 指定的 worktree 内 | ✅ 可写 | ❌ Read-only file system |
| `/tmp` | ✅ **可写（唯一共享区）** | ❌ |
| 主 worktree / 兄弟 worktree | ❌ Read-only file system | ❌ |
| `~`、其它仓库（如 `qaqh-tui-app`） | ❌ Read-only file system | ❌ |

拒绝方式是 `EROFS`（挂载层只读）而非权限位 ⇒ 子代理无法 chmod 绕过。

所以**兄弟 worktree 之间是真隔离**，按 worktree 切分即可放心并行。但必须自己兜住这几条：

1. **一个 agent 锁一个 worktree**：`-C <worktree 绝对路径>`，**绝不允许两个 agent 跑同一个 worktree**。
2. **显式指定沙箱**：`-s workspace-write` 或 `-s read-only`。
   **永不用** `--dangerously-bypass-approvals-and-sandbox`（那才是真无沙箱）。
3. **`/tmp` 是唯一共享可写区 ⇒ 必须按 agent 命名空间**：`-o /tmp/<批次名>-report.md`，
   prompt 里的临时文件也一律带批次前缀，否则并行 agent 会互相覆盖产出。
4. **prompt 里写死禁区**：不得碰主 worktree、其它 worktree、`.git/`、`~/.codex/`、
   `~/.config/qaqh/`；`docs/buglist/` 只有批次 9 可动。
   （沙箱已挡住**写**，但**读**是开放的——要防的是子代理读了别人未完成的中间态而得出错误结论。）
5. **禁止子代理做远端写操作**：`git push`、`cnb pulls post-pull` / `merge-pull` /
   `post-pull-review` 一律由主代理统一执行，避免并发写远端与重复建 PR。
6. 并行上限 4（含主代理），超出排队。

### 4.2 命令模板

```bash
# 只读复核（不改代码）
/home/qaqtamsy/Desktop/ws/codex exec -s read-only -C <worktree 绝对路径> \
  -o /tmp/<name>-report.md "$(cat <prompt-file>)"

# 需要落盘的修复批次：workspace-write + 锁定单个 worktree
/home/qaqtamsy/Desktop/ws/codex exec -s workspace-write -C <worktree 绝对路径> \
  -o /tmp/<name>-report.md "$(cat <prompt-file>)"
```

下发约定：

- 一个批次一次调用；prompt 里**只给该批次的条目原文 + 验收命令**，不重述背景。
- 子代理产出落 `/tmp/`，勾选与结论回写到本目录的 checklist（`[x] … → {commit}`）。
- 沙箱 read-only 时子代理无法写文件，报告走 `-o` 落盘；不要让它试图写仓库。

## 5. 远端流程（CNB）

**分工：issue 看板在本地（本目录），合并请求走 CNB 的 PR。**

### 5.1 凭据（一次性配置，已配好）

读是匿名可用的，但 **push 需要凭据**。`cnb` 自带 credential helper，需显式接进 git
（注意必须用 node + js 的绝对路径，git 调用时环境里未必有 node）：

```bash
cd /home/qaqtamsy/Projects/qaqh-backend
git config --local credential.https://cnb.cool.helper \
  '!/home/qaqtamsy/.local/share/fnm/node-versions/v26.8.2/installation/bin/node \
   /home/qaqtamsy/.local/share/fnm/node-versions/v26.8.2/installation/lib/node_modules/@cnbcool/cnb-cli/bin/cnb.js git-credential'
```

token 在 `~/.cnb/token`，过期时重跑 `cnb login`；`cnb status` 查登录态。

### 5.2 worktree 约定

- 位置：`/home/qaqtamsy/Projects/qaqh-backend/.codex/worktree/<name>`（该目录已被 gitignore）
- 从**最新 main** 切分支：`git worktree add .codex/worktree/<name> -b fix/bug-YYYY-MM-DD-NN-<简述> main`
- 用完 `git worktree remove <path>`；分支合并后 `git worktree prune`
- **一个批次一个 worktree**，与 §4.1 的「一个 agent 锁一个 worktree」对应

### 5.3 PR 流程

```bash
# 1) 在 worktree 里完成改动 + 本地验收（云端不跑 cargo，见 .cnb.yml 注释）
# 2) 推送分支
git -C .codex/worktree/<name> push -u origin <branch>
# 3) 建 PR
cnb pulls post-pull --repo QAQ-Harness/qaqh-backend \
  --base main --head <branch> --title "fix(scope): 中文描述（Closes #N）" \
  --body-file /tmp/pr-body.md
# 4) 看 CI / 自动审查
cnb pulls list-pull-commit-statuses --repo QAQ-Harness/qaqh-backend --number <N>
cnb pulls list-comments --repo QAQ-Harness/qaqh-backend --number <N>
# 5) 合并
cnb pulls merge-pull --repo QAQ-Harness/qaqh-backend --number <N> --merge-style squash
```

**每个 PR 都会自动收到一条 NPC 代码审查评论**（`.cnb.yml` 的 `pull_request` 触发器，免费、只评论）。
PR body 按 `.cnb/settings.yml` 的要求写：改动点 / 回归测试清单 / 验证命令与实际输出摘要 / 来源文档链接。

### 5.4 验证纪律

`.cnb.yml` 明确 **`rust-ci` 已下线**（云端 cargo test/clippy 按量计费），验证一律本地跑：

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Windows-only 路径在 Linux 上无法运行验证的，必须在 PR body 明确标注「未做 Windows 运行时验证」。

## 6. 与其它目录的关系

| 目录 | 职责 |
|---|---|
| `docs/buglist/` | 缺陷登记与状态跟踪（一行一条） |
| `docs/report/` | 证据与结论（长文，只读快照） |
| `docs/plan/` | 设计/方案 |
| `docs/spec/` | 规格化契约 |
| **`docs/todo/`** | **report → 可执行 checklist，供 codex-cli 直接消费** |

## 7. 本轮并行下发的实测补充（2026-09-17，批次 1~4 跑完）

§4.1 的沙箱边界成立（兄弟 worktree 之间确实真隔离、`/tmp` 是唯一共享可写区），
但下面四条是**下发时没写、踩到才知道**的，后续按此下发：

1. **子代理在 worktree 里 `git add` / `git commit` 会直接 EROFS**。
   worktree 的 `.git` 是一个指向主仓库 `.git/worktrees/<name>/` 的**文件**，
   写 `index.lock` 落在主仓库 `.git/` 下 —— 在沙箱可写根之外。
   ⇒ **提交、推送、建 PR、合并一律由主代理做**；prompt 里要显式禁止子代理
   `git commit`（否则它会浪费轮次去试，batch1 的子代理就是这么卡住的）。
2. **不要让子代理回写 checklist**。一个批次一个 worktree ⇒ 每个分支各改一份
   `docs/todo/*-checklist.md`，合并时必然冲突。**勾选由主代理在 main 上统一回写**
   （`[x] … → {squash commit}（PR #N）`），子代理只在 `/tmp/<批次>-report.md` 里给状态表。
3. **并发上限按「子代理 3 + 主代理 1」用满**：3 个 `codex exec` 并行时 CPU 已吃满，
   再叠加主代理的 cargo 会明显拖慢（本轮 `cargo test -p qaqh-workspace` 从 ~30s 涨到 ~2min）。
   每个 worktree 自带 `target/`（12~26 GiB），**合并后立刻 `git worktree remove --force` 回收**，
   否则 6 个 worktree 就能吃掉 ~70 GiB。
4. **`git worktree remove` 需要 `--force`**（`target/` 是 ignored 但仍是未跟踪内容），
   且本环境**不允许 `rm -rf`**——直接 `git worktree remove --force <path>` 即可连 `target/` 一起删。

### 7.1 两条影响「怎么写工具文案」的事实（供后续批次复用）

- **模型可见通道只有 `ToolResult::render_xml_envelope()`**：
  `body = model.text`，而 `model.text` 来自 `error_with(code, message, ..)` 的 **`message`**
  （即 `EngineError` 的 `Display`，上限 `TOOL_MODEL_MAX_CHARS`）。
  **`ToolError.hint` 不进模型通道**（`project_for_model` 也不携带它），
  它只在 ToolResult JSON 里、且被 `TOOL_SUMMARY_MAX_CHARS`(512) 截断。
  ⇒ 任何「要让模型照做」的文案必须落进 `message`/`Display`，写在 hint 里等于没写
  （PR #87 的评审阻断项即此）。
- **`error.message` 字段本身也是 512 截断的**，只有 `model.text`（= 传入的 `message` 原串）
  走大预算。别把长清单塞进 `error.message` 字段的字段级投影里。

### 7.2 环境注意

- 本会话里 Codex 自带的 `apply_patch` 工具在此环境**直接 abort**（连 `/tmp` 下的 ASCII 文件
  都失败），编辑一律用 `edit_file` / `write_file`。与本仓 `qaqh-workspace` 的
  `apply_patch` 工具（批次 3 修的那个）是两码事。
- `cnb pulls merge-pull` **必须带 `--commit-title`**（否则 400 `commit_title is required`）；
  `cnb pulls list-pull-comments` 的正确命令名是 `list-pull-comments`（`--repo` + `--number` 均必填）。
- NPC 自动评审有时**只写 review 不写 comment**（`cnb pulls list-pull-reviews` 看得到），
  有时整条流水线在 6~10s 内 `error`（基础设施抖动）——**空推一个 commit 即可重触发**。
- ⚠️ **2026-09-17 傍晚实测：评审服务连续故障约 1 小时**（#90~#95 六个 PR 全部 6~10s `error`，
  其中包含一个**纯文档 PR**，可判定与 diff 无关；空推重触发也无效）。
  当时按「评审只评论、不阻塞」的既有约定**以本地验证合并**，并在每个 PR 上留言说明原因。
  下次遇到同类情况：先看是否连纯文档 PR 也失败 → 是则等一段时间或直接按本地验证合并，
  **不要为了等评审无限期挂住**；合并后把「本轮无自动评审」写进 PR 评论与 checklist 的勾选注记。
