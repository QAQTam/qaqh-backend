# handoff：buglist 全量复核 → 九批次修复执行（2026-09-17）

> 交接对象：下一个接手 qaqh-backend 开发循环的 agent 或人。
> 关联：
> 复核报告 [`docs/todo/2026-09-17-buglist复核-report.md`](../todo/2026-09-17-buglist复核-report.md)、
> 执行清单 [`docs/todo/2026-09-17-buglist复核-checklist.md`](../todo/2026-09-17-buglist复核-checklist.md)、
> 目录约定与操作手册 [`docs/todo/README.md`](../todo/README.md)、
> 流程手册 [`2026-09-13-CNB-NPC全流程开发管线-handoff.md`](./2026-09-13-CNB-NPC全流程开发管线-handoff.md)

---

## 0. 最终状态快照（2026-09-17 18:30）

- **`main = 7e34226`**，工作区干净（`git status --short` 空），`git worktree list` 只剩主工作区。
- **全量测试绿**：`QAQH_DATA_DIR=$(mktemp -d) cargo test --workspace` → **90 个 test 目标全 ok、0 failed**
  （必须给 `QAQH_DATA_DIR`，见 §5.4）。
- **open PR = 0，open issue = 0**。本轮 9 个 PR `#87~#95` 全部 `is_merged: true`。
- **`docs/todo/` 是本轮新立的公共文档层**（report + checklist + README），进度以 checklist 为准：
  **33 条批次条目全部勾选**，余 12 条 = 5 条阻塞项（B-1~B-5）+ 7 条后续待办（N-1~N-7）。
- 本地已删 9 个已合并分支；**远端仍有 56 个分支**（含本轮 9 个 + 更早几轮积压 38 个 `fix/bug-2026-09-1*`），
  未清理——原因与建议见 §6。

## 1. 一句话交接

复核出的 **29 条仍在生产 + 5 条状态过期**已按 9 个批次全部修完、走 CNB PR 合并进 main，checklist 全勾；
**但有两件事没做完**：① **N-5 是未闭合的 P0 安全**（`exec` 在 Level 4 仍可写工区外路径，收口需要产品口径决策）；
② T-2-3 是**破坏性契约变更**（`/debug` 桥改为 nonce 兑换，仓外 webui/壳层必须迁移）。

## 2. 复核结论（回答「哪些早落盘、哪些还在生产」）

20 份 buglist 共 **91 条**：

| 分类 | 条数 | 处置 |
|---|---|---|
| 已修复且状态正确 | 54 | 不动 |
| **状态过期**（早落盘仍标「工作区，待提交」） | 5 | 批次 9 归档 |
| 部分修复 | 2 | 批次 7 / 批次 9 更正为 `PARTIAL` |
| **仍在生产** | 29（4 条 P0 安全、3 条 P0/P1 子代理取消、6 条工具契约…） | 批次 1~8 修完 |
| 条目描述与 HEAD 不符 | 1 | 批次 9 标「HEAD 不成立」 |

## 3. 批次 → PR → commit 映射

| 批次 | 主题 | PR | main commit | 备注 |
|---|---|---|---|---|
| 1 | 子代理取消链（5 条） | #88 | `629637d` | 取消后不再注入、取消原因位、取消传播、活表单调性 |
| 2 | 安全 P0（4 条） | #93 | `238331f` | **T-2-2 只做文件型，P0 未闭合 → N-5** |
| 3 | `apply_patch` 契约（3 条） | #87 | `1705449` + `cdff8e3` | 评审阻断项由主代理修（见 §4.1） |
| 4 | `edit` 契约（3 条） | #89 | `61b39d0` | T-4-2 选「要求 `old` 覆盖整行」保守方案 |
| 5 | 计量与超限（2 条） | #90 | `006b2b3` | **T-5-2 半条：`context_window` 字段未做 → N-1** |
| 6 | 热重载与 MCP 文案（2 条） | #92 | `b4851b0` | 真机复测半条待做（§5.5） |
| 7 | 零散修复（2 条） | #94 | `c627a1b` | LSP 连接表选惰性摘除 |
| 8 | 安全 P1/P2 收尾（3 条） | #95 | `440a608` | MCP 并发 64→16；**Level 4 仍放行 Exec/Net** |
| 9 | 清单归档与卫生（4 条） | #91 | `563f1e3` | 纯文档 |

看板自身的提交：`68babd2`（并行下发实测）→ `e5462b9`/`ca183d1`/`b72c2ba`/`c1c4836`/`693ff07`（逐批回写）→ `7e34226`（最终验证补记 N-6/N-7）。

## 4. 必须知道的三件事

### 4.1 PR #87 的自动评审抓到一个真问题（已修）

T-3-2 原本把「已生效 / 未生效」清单写进 `ToolError.hint`。实测证明 **`hint` 根本不进模型通道**：

```
模型读到的 = ToolResult::render_xml_envelope() = error_code + model.text
model.text = error_with(code, message, ..) 的 message（= EngineError 的 Display）
ToolError.hint → 只在 ToolResult JSON 里，project_for_model 也不携带，且被 512 截断
```

原实现只改了展示面，模型看到的仍是一句「找不到上下文」，照旧重发整个 patch。
最终落法：清单写进 `EngineError::Partial` 的 `Display`，hint 只留 <512 的行动指引，
并新增 20 文件规模的尾部不截断回归锁。
**推论（写工具文案时通用）**：要让模型照做的文案必须落进 `message`/`Display`，写 `hint` 等于没写。

### 4.2 T-2-3 是破坏性契约变更（仓外消费者必须迁移）

```
GET  /debug/__qaqh_bridge__.js   -> window.__QAQH_DEBUG__={"nonce":"<64hex>"};   ← 不再有 token
POST /debug/__qaqh_token__       body {"nonce":"<64hex>"}
     -> 200 {"token":"<daemon token>"}   (no-store, 一次性, TTL 60s)
     -> 403 {"code":"invalid_nonce"|"cross_site"}
```

- 仓内无消费者（grep 只命中 daemon 自身与 docs），但**仓外 webui/壳层**若仍读
  `window.__QAQH_DEBUG__.token` 会拿到 `undefined` → **全部 API 401**。
- `/health` 响应体去掉 `token_len`（任何解析它的探活/诊断脚本需改）。
- `AppState` 新增 `debug_nonces` 字段 ⇒ 对仓外构造 `AppState` 的代码是 breaking change。

### 4.3 本轮有 6 个 PR 是「无自动评审」合并的

NPC 自动评审流水线在 2026-09-17 17:05~18:10 左右**连续故障**（`#90~#95` 六个 PR 全部 6~10s 内
`error`，其中含一个**纯文档 PR**，可判定与 diff 无关；空推重触发也无效）。
按「评审只评论、不阻塞」的既有约定，这 6 个 PR 以本地验证合并，并在每个 PR 上留言说明。
**如需补评审**：现在服务可能已恢复，可对已合并分支重新开 PR 或直接看 diff。

## 5. 剩余待办

### 5.1 🔴 N-5（P0，安全，**需先定产品口径**）

`exec` 在 Level 4 可写工区外路径——**安全审查 P0-2 的原始攻击面仍未闭合**。

- 为什么没顺手修：`is_path_in_workspace` 只能从 `ctx.args["path"]` 判定目标，而 `exec` 的 schema
  **没有 `path`**（只有 `command`/`argv`/`cwd`）。字面 fail-closed 会在**所有权限等级**阻断 `exec`
  （实测打红 `execution::plan_mode_blocks_destructive_but_not_reads` 与
  `permission_lifecycle::llm_four_pending_bash_calls_defer_execution_until_all_resolved`）。
- 两条路，**都要产品决策**：
  - **(a) 权限层收口**：`needs_permission`/`authorize_call` 里 Level 4 不再无条件放行 Exec/Net
    ⇒ 会新增审批弹窗，改变日常使用体感。
  - **(b) 给 `exec` 加沙箱**：工作区外写入在系统调用层被拒，改动面更大。
- 落点：`crates/qaqh-workspace/src/{manager,permission,safety}.rs`、`crates/qaqh-workspace/src/execution.rs`。

### 5.2 阻塞项（B-1~B-5，**需真实环境，不要派子代理**）

| ID | 内容 | 缺什么 |
|---|---|---|
| B-1 | `exec` 管道文件 7 条未闭环项（`H1a/H1b`、`§C/§E.2/§E.3/§H/§A.2`） | Windows 11 + 安装版 daemon + `%TEMP%\qaqh-exec-probe\` 工具集；`pipe_scan2.rs` 需先超时化 |
| B-2 | 09-14 清单 O-2（TUI 侧非 bash progress 归一/限长） | TUI 仓可读访问；`59541dd` 在本仓不存在 |
| B-3 | 性能收益复测（7 组基准） | 可跑基准的环境 |
| B-4 | 安全并发审查 P2/P3 剩余行 | 需重判（`ACTOR_WORKSPACE` 那条已被 `ActorToolScope` 部分缓解）+ 行号重定位 |
| B-5 | D-2 审批面板可见性（Level 3 弹窗能否看到 `exec` 目标路径） | 实机点击验证 |

### 5.3 后续待办（N-1~N-7，都是小活，可一批派完）

| ID | 内容 | 落点 |
|---|---|---|
| N-1 | profile schema 增 `context_window`（T-5-2 未完成半条） | `crates/qaqh-types/src/config.rs`、`crates/qaqh-config/src/config.rs` |
| N-2 | `apply_patch` 三处收尾：`WOULD_OVERWRITE` hint 措辞分场景、`file_state` 是否需同步、`OVERWROTE` 多 Add File 漏报 | `crates/qaqh-workspace/src/apply_patch*.rs` |
| N-3 | `list_resources` 三态拆分（拉取失败 ≠ 未连接） | `crates/qaqh-mcp/src/connection.rs` + `resources.rs` |
| N-4 | `apply_patch` 解析边界：`*** End Patch` 被当文件名、空文件名 `pop()` 掉 workspace 根、符号链接 workspace 被误拒 | `crates/qaqh-workspace/src/apply_patch_engine/` |
| N-5 | 见 §5.1（P0） | — |
| N-6 | `todo_contract` 测试不隔离，复用数据目录会假红 | `crates/qaqh-workspace/tests/todo_contract.rs` |
| N-7 | README §5.4 的严格 clippy 在 main 上本就跑不过 | `crates/qaqh-config-api/src/lib.rs:316`、`crates/qaqh-message/src/wal.rs:609` |

## 6. 远端分支积压（**2026-09-17 晚已清理**）

远端 56 个分支里 38 个是 `fix/bug-2026-09-1*`（本轮 9 个 + 更早几轮积压）。
**坑**：这些都是 **squash 合并**，所以 `git branch --merged main` / `merge-base --is-ancestor`
**判定不出来**（分支 commit 不是 main 的祖先）；只能靠 CNB PR API 的 `is_merged: true` 确认内容已落盘。
删除的代价是分支上的原始 commit 对象变为不可达（PR 里仍有完整 diff 与讨论）。

**执行结果（2026-09-17 晚，机主指示清理）**：

- 逐分支核对了两条独立判据（① `git merge-base --is-ancestor origin/<b> main`；
  ② `cnb pulls list-pulls --state all` 里该 head 的 `mergeable_state == merged`），
  **55 个分支两条至少一条成立** ⇒ 一次 `git push origin --delete <55 refs>` 删除完毕。
- **唯一未删**：`perf/bug-2026-09-13-28-block-checkpoint`——它的 PR **#77 是 closed 而非 merged**，
  关闭留言写明「让位同 issue 的 #78」（预检发现 v1 的字节闸退化成每 token 一发，属功能回归；
  v2 = #78 已合并）。**内容是被主动否决的实现**，删不删等机主拍板（当前保留）。
- 删后：远端只剩 `main` + 上面那一个分支；`main` 未变；open PR 仍为 0（没有在跑的 PR 依赖被删分支）；
  抽查 #87 等已删分支的 PR 记录与 diff 仍可读。
- 本地还剩两个已合并分支（`fix/bug-2026-09-17-followups`、`docs/todo-sandbox-verified`），
  它们的远端副本已随本轮删除；本地副本是**目前唯一**保存 pre-squash commit 对象的地方，故未动。

## 7. 继续推进的操作手册（照抄即可）

### 7.1 worktree 与沙箱

- 位置 `.codex/worktree/<批次名>`（已 gitignore），**一个批次一个 worktree**，从最新 main 切：
  `git worktree add .codex/worktree/<name> -b <branch> origin/main`
- 子代理沙箱实测边界（`docs/todo/README.md` §4.1/§7）：worktree 内可写、`/tmp` 可写且是**唯一共享区**、
  兄弟 worktree/主 worktree/`~` 全部只读（EROFS，非权限位，绕不过）。
- 用完 **`git worktree remove --force <path>`**（`target/` 是未跟踪内容，必须 `--force`；
  本环境**不允许 `rm -rf`**）。合并后立刻回收——每个 worktree 的 `target/` 有 12~26 GiB。

### 7.2 下发 codex-cli 子代理

二进制**只用** `/home/qaqtamsy/Desktop/ws/codex`（其余路径是旧版实现）：

```bash
cd <worktree> && setsid nohup /home/qaqtamsy/Desktop/ws/codex exec \
  -s workspace-write -C <worktree 绝对路径> \
  -o /tmp/<批次>-report.md - < /tmp/<批次>-prompt.md > /tmp/<批次>.log 2>&1 &
```

prompt 必须写死的约束（本轮实测踩过）：

1. **只在 worktree 内操作**，不碰主 worktree / 其它 worktree / `.git/` / `~/.codex/` / `~/.config/qaqh/`。
2. **不要改 `docs/buglist/` 与 `docs/todo/`**——多分支并行各改一份 checklist 必然冲突，
   **勾选由主代理在 main 上统一回写**。
3. **不要做任何 git 写操作**（`add`/`commit`/`reset`/`checkout --`/`stash`/`push`），不要调 `cnb`。
   两个实测教训：① worktree 的 `.git` 指向主仓库，**`git add` 直接 EROFS**，提交只能主代理做；
   ② 有一个子代理用 `git reset` 把自己的未提交改动**全抹了**，靠事前 `git diff > /tmp/<批次>-my.patch`
   才恢复——所以要么禁掉 git 写操作，要么要求先存 patch。
4. 临时文件统一 `/tmp/<批次>-` 前缀（`/tmp` 是并行 agent 的共享区）。
5. 只改本批次列出的文件，显式列出**不要动**的兄弟批次文件。
6. 验收要「命令 + 期望输出」，要求**原样粘贴**；失败如实写，**不许声称通过**。

并发上限 **4（含主代理）**，即最多 3 个子代理并行。

### 7.3 CNB 命令（本轮实际用到的）

```bash
export PATH="$PATH:/home/qaqtamsy/.local/share/fnm/node-versions/v26.8.2/installation/bin"
cnb pulls post-pull --repo QAQ-Harness/qaqh-backend --base main --head <branch> \
  --title "…" --body-file /tmp/pr-body-<批次>.md
cnb pulls list-pull-commit-statuses --repo QAQ-Harness/qaqh-backend --number <N>
cnb pulls list-pull-reviews        --repo QAQ-Harness/qaqh-backend --number <N>   # 评审可能只写 review 不写 comment
cnb pulls list-pull-comments       --repo QAQ-Harness/qaqh-backend --number <N>
cnb pulls merge-pull --repo QAQ-Harness/qaqh-backend --number <N> \
  --merge-style squash --commit-title "<标题> (#N)"          # ⚠️ 必须带 --commit-title，否则 400
```

PR body 按 `.cnb/settings.yml`：改动点 / 回归测试清单 / 验证命令与实际输出摘要 / 来源文档链接。

### 7.4 验证纪律（本地跑，**云端不跑 cargo**）

```bash
QAQH_DATA_DIR=$(mktemp -d) cargo test --workspace     # ← 必须给这个变量，见下
cargo clippy --workspace --all-targets               # ← 不加 -D warnings，见 N-7
cargo fmt -p <crate> -- --check
```

**为什么必须给 `QAQH_DATA_DIR`**：默认数据目录是 `~/.config/qaqh`。① 在子代理沙箱里它只读，
`ask_user_lifecycle` 等用例会以 `session_resume_failed` 假红；② 在主代理侧直跑会**写到机主的真实会话目录**。
给临时目录既能跑通又能隔离。注意**每次都要换新目录**，否则 N-6 的 `todo_contract` 会假红。

`.cnb.yml` 明确 `rust-ci` 已下线（云端 cargo 按量计费烧真钱）——**任何批次都不许在云端跑构建**。

## 8. 本轮未做 / 不确定性

- **N-5 未闭合**（P0，见 §5.1）——这是本轮最大的遗留。
- T-5-2 只做了本地 pre-flight + 端点 400 回收，`context_window` 字段没做（N-1）；
  `run_auto_compact → ContinueTurn → 重发`的完整回收链路**无自动测试**。
- T-6-1 的「重启 daemon 后第一次改 `[lsp]` → 日志出现 `[lsp] hot-reload applied`」**未做真机复测**
  （沙箱起不了 daemon、改不了 `~/.config/qaqh/config.toml`）；可验证部分已由回归测试锁定。
- T-8-1 的实际边界：消除的是「MCP 工具**独有**的 allow-all 特权」；**默认档 Level 4 下 Exec/Net 动态工具
  仍自动放行**（与内置工具一致）。要「任何档位都弹审批」需单独调整全局档位策略。
- 批次 8 执行探针时**误发过一次真实线上调用**：`PATCH https://api.cnb.cool/evil/-/issues/1` → 404
  （仓库不存在，无副作用）。**教训：让子代理跑 MCP 探针时，必须要求离线短路，别给真实 repo 名。**
- 本会话里 Codex 自带的 `apply_patch` 工具**在此环境直接 abort**（连 `/tmp` 下的 ASCII 文件都失败），
  编辑一律用 `edit_file`/`write_file`。与本仓 `qaqh-workspace` 的 `apply_patch` 是两码事。
- `/tmp` 里还留着约 23MB 的 `qaqh-*` 测试夹具（几百个小目录），是 `cargo test` 每次跑都会生成的，
  非本轮特有，未清。

---

## 9. 接手后追加（2026-09-17 19:30，后续待办收尾）

> 本节由接手 agent 追加；§0~§8 是上一轮的历史快照，未改动。

- **`main = 3749df1`**（`fix(workspace,mcp,config,runtime): 后续待办 N-1~N-4/N-6/N-7 收尾 (#96)`），
  工作区干净、本地无 worktree。
- **§5.3 的 N-1/N-2/N-3/N-4/N-6/N-7 全部落盘**（PR #96）。⚠️ **本轮又是「无自动评审」**，
  根因已查明 = **组织级 CPU 配额不足**（不是评审链路、不是 NPC 角色/召唤写错）：

  ```
  $ cnb build get-build-stage --repo QAQ-Harness/qaqh-backend --sn cnb-028-1k2nih8fo \
        --pipelineId cnb-028-1k2nih8fo-001 --stageId prepare
  Pipeline prepare error: Root Group's events CPU core-hours are insufficient for pre-freezing
    (Freezing time:5.00 min,equivalent to 0.67 core-hours). Contact the root group administrator to extend.
  根组织的云原生构建-CPU配额已不够预冻结(冻结时间：5.00 min，折合0.67核时)，请联系根组织管理员提升配额。
  ```

  阶段表显示 **`Prepare` error、`npc go` skipped**（NPC 从未启动）；`main.push` 的
  `build-npc-image`（与角色无关）也是同一条报错。正向对照：`cnb-n2h-1k2nb4trh`（09:28:47Z）
  抢到 runner，`npc go` 成功 572s，PR #92 随后收到 NPC 评审评论 ⇒ 链路可用，
  **能否跑到取决于当时配额余量**。按既有约定以本地验证合并，证据留在 PR #96 评论。
  （处置：根组织管理员提额 / 等配额周期刷新；补评审可用 `api_trigger_wm` 事件。）
- 与清单原文的**三处修正**（都是「先复现再动手」的结果，细节见 checklist 的 N-2/N-4/N-7 条目）：
  - **N-4① 是真问题**：`*** Add File: *** End Patch` 修前会返回 `Ok` 并**真的建出名为
    `*** End Patch` 的文件**；已加四头守卫（Add/Delete/Update/Move to）+ 4 条回归锁。
  - **N-4②③ 与 N-2②③ 实测不成立**（HEAD 上已是正确行为：空文件名本来就 `PARSE_ERROR`、
    符号链接 workspace 的绝对路径能正常解析、`file_state` 已同步、多 Add File 覆盖已逐条上报），
    因此只补回归锁、不动代码。
  - **N-7 的 `wal.rs:609 io_other_error` 不再复现**（1.98.1 上首参是变量而非字面 `ErrorKind::Other`），
    实际报的是 `wal.rs:1057/1070` 的 `assert_eq!(x, true)`。最终清掉 main 上 **23 条**既有 warning，
    并把 `cargo fmt --all` 的既有漂移（`debug_control.rs` 等 7 个文件，仅换行）一并修掉
    ⇒ **§7.4 的四条验证命令现在可以直接照抄**（`README.md` §5.4 已同步更新）。
- **仍未闭合（需要机主决策，agent 不能自己拍）**：
  1. 🔴 **N-5（P0 安全）**：`exec` 在 Level 4 仍可写工区外路径。两条路都要产品口径：
     **(a) 权限层收口**（Level 4 不再无条件放行 Exec/Net，会新增审批弹窗，改变日常体感）；
     **(b) 给 `exec` 加沙箱**（系统调用层拒绝工区外写入，改动面大）。
  2. **§6 远端分支积压** —— ✅ **已按机主指示清理**（55 个已合并分支删除；只剩一个
     closed-not-merged 的 `perf/bug-2026-09-13-28-block-checkpoint`，等机主拍板是否也删，见 §6）。
  3. B-1~B-5（需真实环境，见 §5.2）。
- 顺手记两条操作教训：
  - `rg -rn "<pat>" <path>` 里的 `-r` 是 **`--replace`**（不是递归），会把匹配替换成字面量 `n`
    打出来——本会话因此一度照着假输出写错函数名。递归是 `rg` 的默认行为，直接 `rg -n "<pat>" <path>`。
  - 新增/改动 clippy 相关代码后要**重跑** `-D warnings`：本会话第一轮全绿后，新加的测试里
    `let mut cfg = Config::default(); cfg.x = …` 又触发了 `field_reassign_with_default`。
