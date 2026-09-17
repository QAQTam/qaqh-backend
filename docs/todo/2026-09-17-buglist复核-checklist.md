# buglist 复核 — 执行清单（2026-09-17）

> 来源：`./2026-09-17-buglist复核-report.md`
> 基线：`661e5f4`；工作树 `crates/` 零改动。所有行号以该 commit 为准。
> 用法：**一个批次 = 一次 codex-cli 下发**。批次内条目按序执行；批次之间互不依赖，可并行。
> 二进制：`/home/qaqtamsy/Desktop/ws/codex`（**只用这个路径**，其余是旧版实现）。
> 勾选规则：`[x]` 之后必须补 `→ {commit}` 或 `→ {验收命令原文}`，禁止空勾。

## 进度总览

| 批次 | 主题 | 条目数 | 严重度 | 依赖 | 状态 |
|---|---|---|---|---|---|
| 1 | 子代理取消链 | 5 | P0/P1/P2 | 无 | ☑ 629637d (#88) |
| 2 | 安全 P0（越权/边界/凭据/泄漏） | 4 | P0 | 无 | ☑ 238331f (#93)，**T-2-2 部分（见 N-5）** |
| 3 | `apply_patch` 契约 | 3 | P0/P1 | 无 | ☑ 1705449 (#87) |
| 4 | `edit` 契约 | 3 | P0/P1 | 无 | ☑ 61b39d0 (#89) |
| 5 | 计量与超限 | 2 | P1 | 无 | ☑ 006b2b3 (#90)，T-5-2 部分（见 N-1） |
| 6 | 热重载与 MCP 文案 | 2 | P1/P2 | 无 | ☑ b4851b0 (#92)，T-6-1 真机复测待做 |
| 7 | 零散修复 | 2 | P1 | 无 | ☑ c627a1b (#94) |
| 8 | 安全 P1/P2 收尾 | 3 | P1/P2 | 批次 2 | ☑ 440a608 (#95) |
| 9 | 清单归档与卫生（文档） | 4 | — | 无 | ☑ 563f1e3 (#91) |
| — | 阻塞项（需现场环境，不派） | 8 | — | — | 🚫 |

---

## 批次 1：子代理取消链 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-runtime/src/registry.rs`、`crates/qaqh-subagent/src/lib.rs`、`crates/qaqh-runtime/src/agent/loop_injection.rs`、`crates/qaqh-runtime/src/agent/engine_input.rs`、`crates/qaqh-workspace/src/process_registry.rs`、`crates/qaqh-runtime/src/ringing/hub.rs`
- 说明：这批是机主报告的「已取消的子代理怎么复活了」的完整根因链。清单自述 T-1-1 是 1 行、T-1-2 是 1 个守卫。

- [x] **T-1-1 子代理 spawn 时注册 liveness**（P0）→ 629637d（PR #88）
  - 位置：`crates/qaqh-runtime/src/registry.rs:361-430`（`spawn_subagent_inprocess`）
  - 现状：全文无 `hub.mark_worker_live(seed)`；`hub` 在 `:386` 被 clone 后只交给 event reader 线程。对照 `:292`（`get_or_spawn`）与 `:715`（`respawn_dead_agents`）都有。⇒ 子 seed 不在 `live_workers`，bootstrap 的 orphan seal 会把它判为孤儿封禁（E2）
  - 动作：在 `:425` 插入 `self.instances` 之后补 `self.hub.mark_worker_live(seed);`
  - 验收：`rg -n 'mark_worker_live' crates/qaqh-runtime/src/registry.rs` → 出现 3 处（含 `spawn_subagent_inprocess`）；新增单测 `spawn_subagent_registers_liveness`；daemon 日志出现 `worker alive for {seed}` 而非 `sealing orphan active turn`
  - 关联：D-5 / BUG-2026-09-17-01

- [x] **T-1-2 取消后不再注入结果**（P0）→ 629637d（PR #88）
  - 位置：`crates/qaqh-subagent/src/lib.rs:533-595`
  - 现状：`did_cancel` 只在 `:523` 决定 `state_tag` 文案；`:543-586` 的注入块无守卫，取消后仍把完整 `final_answer` 注入父会话。`registry_ref.finish()` 在注入**之后**（`:597`）（E2）
  - 动作：`:533` 条件改为 `if !parent_seed.is_empty() && !did_cancel`；若产品上要留痕，改为注入**不含 `final_answer`** 的状态行
  - 验收：新增单测 `cancelled_collector_does_not_inject`；cancel 路径日志不再出现 `inject accepted`
  - 关联：D-6 / BUG-2026-09-17-02

- [x] **T-1-3 系统注入区分「用户取消」与「回合取消」**（P1）→ 629637d（PR #88，落点在 Loop 层 `loop_injection.rs`）
  - 位置：`crates/qaqh-runtime/src/agent/loop_injection.rs:196-220`、`crates/qaqh-runtime/src/agent/engine_input.rs:261-262`
  - 现状：`LoopPhase::Idle` 分支无条件 `handle_system_input` 开新回合；`engine_input.rs` 主动 `ctx.cancel.clear(); qaqh_workspace::clear_cancel();` ⇒ 任何系统注入都能复活已取消会话（E2）
  - 动作：引入取消原因位（`cancel_reason` / `user_cancelled`）；系统注入仅在**非用户取消**态清除标志并开回合
  - 验收：取消后父会话不再出现新的 `TurnStart`；单测覆盖「用户取消后系统注入被拒」
  - 关联：D-7 / BUG-2026-09-17-03

- [x] **T-1-4 父会话取消传播到子 seed**（P1）→ 629637d（PR #88，落点 `AgentRegistry::send_ringing`）
  - 位置：`crates/qaqh-runtime/src/registry.rs`（新增登记）、`crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs:95-124`（取消处理）
  - 现状：全仓 `rg -n 'children|descendants|child_seeds' crates/qaqh-runtime/src` 在 cancel 路径零命中；父会话没有「自身子 seed 集合」的登记能力（E2）
  - 动作：spawn 时登记 `parent_seed -> {child_seeds}`，`forget_seed` 时清理；cancel 处理遍历并逐个 `cancel` + `mark_worker_dead`
  - 验收：新增单测 `parent_cancel_propagates_to_children`；daemon 日志显示子 seed 收到取消
  - 关联：D-8 / BUG-2026-09-17-04

- [x] **T-1-5 状态单调性守卫 + 接入 `mark_worker_dead`**（P2）→ 629637d（PR #88）
  - 位置：`crates/qaqh-workspace/src/process_registry.rs:478-486`、`crates/qaqh-runtime/src/ringing/hub.rs:752`
  - 现状：`mark_exited` 无条件 `= ProcStatus::Exited(code)`，会把 `Killed` 覆盖回 `Exited`，违反同文件 `:463-465` 注释自称的单调性；`mark_worker_dead` 全仓**零生产调用点**（仅 `:3417` 一个测试调 `mark_worker_live`），`live_workers` 只靠 `forget_seed`（`:791-793`）清理（E2）
  - 动作：`mark_exited` 加守卫——仅当前状态为 `Running` 时才改写；把 `mark_worker_dead` 接到子代理退出路径（与 T-1-1 同一处改动）
  - 验收：新增单测 `mark_exited_does_not_downgrade_killed`；`rg -n 'mark_worker_dead' crates/` 出现生产调用点
  - 关联：D-9、D-10 / BUG-2026-09-17-05、-07

---

## 批次 2：安全 P0 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-runtime/src/service/fs_git.rs`、`crates/qaqh-runtime/src/service.rs`、`crates/qaqh-workspace/src/manager.rs`、`crates/qaqh-workspace/src/safety.rs`、`crates/qaqh-daemon/src/axum_server.rs`、`crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs`、`crates/qaqh-session/src/manager.rs`

- [x] **T-2-1 `fs.read`/`fs.list` 加路径白名单**（P0）→ 238331f（PR #93）
  - 白名单 = 任一注册 UI workspace ∪ 任一会话 `meta.cwd` ∪ `platform::data_dir()`；
    敏感路径恒拒（含 `sessions` 目录本身），拒绝返回 `FORBIDDEN`。
  - ⚠️ 集成影响：`examples/remote_fs.rs` 默认参数是 `/`，现在会拿到 `FORBIDDEN`；
    HTTP 层错误码从 `query_failed` 变为 `{"code":"forbidden"}`。
  - 位置：`crates/qaqh-runtime/src/service/fs_git.rs:10-64`、`:68-91`；入口 `crates/qaqh-runtime/src/service.rs:232-243`
  - 现状：两个函数只校验 `is_absolute()`，源码注释 `:8-9` 自述「临时跨端版本有意不做路径沙箱/权限校验」⇒ 持 token 者可读列任意绝对路径（E2）
  - 动作：两入口各加 `allowed_roots(workspace_root, data_dir)` 前缀校验（组件级比较，复用 `crates/qaqh-workspace/src/permission.rs:375` 的 `path_within_dir`）；拒绝返回 `FORBIDDEN` 而非 `IO_ERROR`；显式复用 `is_sensitive_session_path` 取代按 `meta.json` 子串过滤；删除 `:8-9` 的过时注释
  - 验收：新增集成测试 `fs_read_rejects_meta_json`、`fs_list_rejects_sessions_dir`，断言返回码非 IO；`cargo test -p qaqh-runtime` 全绿
  - 关联：D-1 / 安全审查 P0-1

- [x] **T-2-2 Destructive 工具缺 `path` 参数时 fail-closed**（P0）→ 238331f（PR #93，**只做了文件型，P0 未闭合**）
  - ⚠️ **清单原文自相矛盾**：`exec` 是 `Destructive` + `ToolCategory::Exec`，其 schema
    **没有 `path` 参数**（只有 `command`/`argv`/`cwd`）⇒ 字面规则「所有 Destructive 缺 path ⇒ false」
    会在**所有权限等级**阻断 `exec`。实测打红
    `execution::plan_mode_blocks_destructive_but_not_reads` 与
    `permission_lifecycle::llm_four_pending_bash_calls_defer_execution_until_all_resolved`。
  - 实际落地：`is_path_in_workspace` 增加 `category` 参数——**文件型** Destructive
    （`Destructive` + `Write`，即 `delete`）缺 `path` 才 fail-closed；Exec/Net 维持原判定。
  - ❌ **仍未闭合**：「`exec` 在 Level 4 可写工区外路径」这半条**依然敞开**
    （`is_path_in_workspace` 无法从 `command` 文本判定目标）⇒ 见 **N-5（P0）**。
  - 位置：`crates/qaqh-workspace/src/manager.rs:525-549`（`is_path_in_workspace`）、`crates/qaqh-workspace/src/safety.rs:14-24`
  - 现状：`is_path_in_workspace` 只读 `ctx.args["path"]`；`exec` 的参数名是 `command`，于是走 `:545-548` 的 `else` 分支**恒返回 `true`**。`exec` 在 `exec/register.rs:29` 声明为 `ToolRisk::Destructive`，而 `safety.rs:18-21` 只在 `(Destructive, false)` 时阻断 ⇒ 判定被短路，永远放行。Level 4（`permission.rs:489-491`）完全自动批准（E2）
  - 动作：`is_path_in_workspace` 的 `else` 分支按 risk 分级——`Destructive` 工具缺 `path` 时返回 `false`（fail-closed）；`Write`/`ReadOnly` 维持 `true` 以免误伤 `task`/`skills`/`ask`
  - 验收：新增单测 `destructive_tool_without_path_is_treated_as_outside_workspace`；e2e 覆盖 Level 4 下 `exec` 写工区外路径 → 期望被 `SafetyPolicy` 阻断
  - 关联：D-2 / 安全审查 P0-2

- [x] **T-2-3 停止泄露 token**（P0）→ 238331f（PR #93）
  - `/health` 不再报 `token_len`；debug 桥只下发 nonce，token 改由
    `POST /debug/__qaqh_token__` 一次性兑换（TTL 60s + `Sec-Fetch-Site` 同源约束）。
  - ⚠️ **破坏性契约变更**：仓外 webui/壳层若仍读 `window.__QAQH_DEBUG__.token`
    会拿到 `undefined` → 全部 API 401，**必须迁移**；`AppState` 新增 `debug_nonces` 字段。
  - 位置：`crates/qaqh-daemon/src/axum_server.rs:14`、`crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs:105-106`
  - 现状：`/health` 返回 `format!("ok epoch={} token_len={}", …)`；debug 桥把明文 `state.token` 注入 `window.__QAQH_DEBUG__`。回环 + Host 双检（`debug_control.rs:283`、`:306`）已在，但 nonce 一次性兑换 / `Sec-Fetch-Site` / 常量时间比较三项加固未做（E2）
  - 动作：`/health` 改为只报 `ok epoch={}`；桥脚本改为**只下发 nonce**，token 由客户端凭 nonce 走一次兑换接口换取、兑换后作废
  - 验收：单测断言 `/health` 响应体不含 `token` 子串；`/debug` 响应体不含真实 token 字面量
  - 关联：D-3 / 安全审查 P0-3

- [x] **T-2-4 `session_locks` 在删除路径释放**（P0）→ 238331f（PR #93，摘除动作在所有其它锁释放之后执行，无反向获取）
  - 位置：`crates/qaqh-session/src/manager.rs:1380-1385`（insert）、`:223-241`（`delete()`）
  - 现状：`delete()` 只做 `invalidate_watermark` / `remove_dir_all` / `remove_from_index` / `remove_session`，**不碰 `session_locks`**；`:1237` 的 `release_seed_claim` 清的是另一个 map（`claimed_seeds`）⇒ 每删一个会话永久多留一条（E2）
  - 动作：`delete()` 末尾加 `self.session_locks.lock()?.remove(seed);`，并同步 `WorkspaceStore::remove_session` 侧；注意与 `session_lock()` 的持锁顺序，避免反向获取
  - 验收：新增单测 `session_locks_shrinks_after_delete`（删前/删后 `len()` 相等）；`cargo test -p qaqh-session` 全绿
  - 关联：D-4 / 安全审查 P0-4

---

## 批次 3：`apply_patch` 契约 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-workspace/src/apply_patch.rs`、`crates/qaqh-workspace/src/apply_patch_engine/mod.rs`、`crates/qaqh-workspace/src/apply_patch_engine/seek_sequence.rs`

- [x] **T-3-1 `Add File` 对已存在路径加守卫**（P0）→ 1705449（PR #87；走清单备选路径——dry_run 报 `WOULD_OVERWRITE` + 真 apply 把旧内容记入 `FileDelta.old`，因拒绝覆盖会打红既有上游 fixture `011_add_overwrites_existing_file`）
  - 位置：`crates/qaqh-workspace/src/apply_patch_engine/mod.rs:185-186`（真 apply）、`:305-317`（dry-run）、`:188-193`（delta `old: None`）
  - 现状：`Hunk::AddFile` 直接 `write_file_with_missing_parent_retry`，无 exists 检查；dry-run 也只拒绝目录。⇒ `dry_run=true` 返回普通 `[DRY RUN] … ok`，真 apply 整文件覆盖且返回体不含 overwrite 字段（**静默数据丢失**）（E2）
  - 动作：`AddFile` 加 exists 守卫；`dry_run` 对已存在路径返回 `WOULD_OVERWRITE`；把旧内容读进 `FileDelta.old` 以便回滚
  - 验收：新增单测 `add_file_refuses_existing_path`；手动复现清单探针（对已存在 5 行文件发 `*** Add File:`）→ 期望拒绝或显式 `WOULD_OVERWRITE`
  - 关联：D-11② / BUG-2026-09-16-10

- [x] **T-3-2 失败结果改为陈述事实**（P1）→ 1705449 + cdff8e3（PR #87）
  - ⚠️ **复核纠正**：清单原写的「失败 hint 改为陈述事实」**不足以修好**——`ToolError.hint` 不进模型通道
    （模型读 `ToolResult::render_xml_envelope()`，body = `model.text` = `error_with` 的 `message`，
    即 `EngineError` 的 `Display`；`hint` 只在 ToolResult JSON 里，`project_for_model` 也不带它）。
    原实现只改了展示面，模型看到的仍是「找不到上下文」，照旧重发整个 patch。
  - 最终落法：清单写进 `EngineError::Partial` 的 `Display`（进 `model.text`，预算 `TOOL_MODEL_MAX_CHARS`），
    `hint` 只留一句 <512 的行动指引；新增用例 `partial_failure_lists_survive_for_many_files`
    锁住列表**尾部**不被 512 截断。详见 PR #87 的评审回复评论。
  - 位置：`crates/qaqh-workspace/src/apply_patch.rs:157`；对照 `apply_patch_engine/mod.rs:181`（`for hunk in &hunks`）+ `:273`（循环体内 `std::fs::write`）
  - 现状：hint 写「Re-send the FULL corrected patch — no partial application happened」，而引擎是逐 hunk 边算边写、非原子；同文件 `:7-9` 的模块文档自述的恰恰是「已写入的文件保留」，两处互相矛盾。模型按提示重发完整 patch 必然二次 `NO_MATCH`（E2）
  - 动作：hint 改为陈述事实（「已生效：a.txt；未生效：b.txt — 修正后**只重发失败部分**，或先 `git diff`/`read` 核对已生效文件」）
  - 验收：新增单测 `failed_hunk_hint_reports_partial_application`；手动复现清单 BUG-09 探针（1 号 hunk 合法、2 号上下文不存在）→ 期望文案列出已生效文件
  - 关联：D-11① / BUG-2026-09-16-09

- [x] **T-3-3 补「上下文充分性」警示**（P2）→ 1705449（PR #87，只补文案；`seek_sequence` 仍取首个命中，与上游 `codex-rs/apply-patch` 一致）
  - 位置：`crates/qaqh-workspace/src/apply_patch_engine/seek_sequence.rs:39-43`；工具描述 `crates/qaqh-workspace/src/apply_patch.rs:188-197`
  - 现状：exact 循环直接 `return Some(i)`，无歧义拒绝（对比 `edit` 会报 `Ambiguous`）；工具描述与格式说明都**没有**「同一上下文多处出现时必须补足上下文或用 `@@` 锚定」这条警示 ⇒ 静默改第一处并返回 `[OK]`（E2）
  - 动作：在工具描述里补该警示（保持「取首个命中」的现有语义不变，只补文案）；若决定改为歧义拒绝，需单独评估与上游 `codex-rs/apply-patch` 的行为差异
  - 验收：`sed -n '188,200p' crates/qaqh-workspace/src/apply_patch.rs` → 文案含歧义要求
  - 关联：D-11③ / BUG-2026-09-16-11

---

## 批次 4：`edit` 契约 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-workspace/src/edit/matching.rs`、`edit/resolve.rs`、`edit/mod.rs`、`edit/handler.rs`、`edit/transaction.rs`

- [x] **T-4-1 描述与 schema 收敛到 3 个 kind**（P1，纯文案）→ 61b39d0（PR #89）
  - 位置：`crates/qaqh-workspace/src/edit/handler.rs:344`（工具描述）、`:356`（hunks schema）；`edit/mod.rs:10`（模块文档）；`edit/transaction.rs:341`（`INVALID_REGEX → replace_inline` 映射）
  - 现状：四处仍列 `insert_after` / `insert_before` / `replace_inline`，实现已在 `a92626d`（2026-09-08）删除 ⇒ 任何按描述发起的调用恒 `PARSE_ERROR: unknown hunk kind '…' (expected replace / prepend_file / append_file)`（E2）
  - 动作：描述与 schema 收敛到 `replace` / `prepend_file` / `append_file`；`transaction.rs:341` 的 `INVALID_REGEX` 提示改为指向 bash/python 做正则替换
  - 验收：`rg -n 'insert_after|insert_before|replace_inline' crates/qaqh-workspace/src/edit/` → 仅剩 `tests.rs` 的历史注释（或无）；新增用例断言按描述发起的调用不再出现在描述里
  - 关联：D-13 / BUG-2026-09-16-08

- [x] **T-4-2 Tier3 采纳后替换区间锚定到 `old` 的行内 span**（P0）→ 61b39d0（PR #89；**选方案 (b)**——窗口剥缩进字数 > `old` 剥缩进字数则跳过该候选，护栏放在 Tier3 打分循环内，真·整行 typo 仍能胜出）
  - 位置：`crates/qaqh-workspace/src/edit/resolve.rs:34-35`（`char_starts[start_line]..char_starts[start_line + win_lines]`）、`edit/matching.rs:102`（`TextDiff::from_chars().ratio()`）、`edit/mod.rs:32`（`T3_THRESHOLD = 0.85`）
  - 现状：字符级评分 ≥ 0.85 即采纳，但替换区间是**整个命中窗口**，不是 `old` 在行内的位置 ⇒ 行内片段 `old` 被采纳时，该行未被 `old` 覆盖的前后缀**无提示删除**，返回仍是 `1/1 hunks applied … score 0.98`（**静默丢内容**）（E2）
  - 动作：二选一——(a) 让 Tier3 的替换区间锚定到 `old` 的实际行内 span；(b) 采纳前要求 `old` 覆盖整行（否则不采纳）。方案 (b) 改动更小、语义更保守，建议优先
  - 验收：新增「片段 old」用例（清单自述 42 个 `#[test]` 里目前**一个都没有**）；`cargo test -p qaqh-workspace edit` 全绿；手动复现清单探针 → 期望不再出现前后缀被删
  - 关联：D-12① / BUG-2026-09-16-06

- [x] **T-4-3 补第四种失败诊断「`old` 是行内片段」**（P1）→ 61b39d0（PR #89）
  - 位置：`crates/qaqh-workspace/src/edit/matching.rs:190-204`（`no_match_detail`）
  - 现状：只有「差阈值 / 差 margin / 完全不像」三种口径。当 `old` 是行内片段且达不到 0.85 时，报「best score 0.32 is below threshold 0.85 — closest location is probably wrong; re-check 'old' against the file」，而 `old` **逐字符就在候选行里** ⇒ 模型据此判定「自己记错了内容」，真因是「片段 vs 整行」（E2）
  - 动作：新增第四种诊断——检测到候选行**包含** `old` 子串时，提示「`old` 是行内片段，需给出完整行内容」
  - 验收：新增用例覆盖该诊断分支；手动复现清单探针 → 文案指向「片段 vs 整行」
  - 关联：D-12② / BUG-2026-09-16-07

---

## 批次 5：计量与超限 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-runtime/src/agent/state/token_calibration.rs`、`crates/qaqh-runtime/src/agent/engine_compact.rs`、`crates/qaqh-runtime/src/agent/engine_turn.rs`

- [x] **T-5-1 图片字节不计入 token 估算**（P1）→ 006b2b3（PR #90；结构化剥离 `ContentBlock::Image.data` 与 `ToolResult.images[].data`，每图固定 4096 token，`count_tokens` 语义未改。修前 1 MiB 截图折算 317,788 token）
  - 位置：`crates/qaqh-runtime/src/agent/state/token_calibration.rs:163-164`；同一盲点第二处 `crates/qaqh-runtime/src/agent/engine_compact.rs:432-435`
  - 现状：把整份 `(messages, tools)` `serde_json::to_string` 后交 `count_tokens`，而 `ToolResult.images[].data` 的内联 base64（`crates/qaqh-types/src/tool_result.rs:83-96`）就在这份字符串里，计数实现（`crates/qaqh-types/src/token.rs:19-28`）无图片感知 ⇒ 1 MiB 截图被折算成二三十万 token，而端点按像素只算几千；带图会话每轮触发 auto-compact，压缩后仍超阈值，prompt cache 反复失效（E2）
  - 动作：序列化前把图片字节替换为等价计费占位符（保守固定上限，改动最小、无契约变更）；`engine_compact.rs:432` 一并处理
  - 验收：新增回归锁断言 `prepared_request_metrics` 不随图片字节线性增长（当前必然失败）；`rg -n 'image|base64' crates/qaqh-runtime/src/agent/state/token_calibration.rs` 出现处理分支
  - 关联：D-14 / BUG-2026-09-16-05

- [x] **T-5-2 超限请求本地 pre-flight**（P1）→ 006b2b3（PR #90，**部分完成**）
  - 已做：`compact_preflight`（`decision_tokens ≥ context_limit` ⇒ `ForcedCompact`，在 `gate_request` 之前）
    + 端点 400 超限回收分支（识别各 provider 文案，上限 2 次，压缩无产出即放弃）。
  - 未做：**profile schema 增 `context_window` 字段**（需改 `qaqh-types/src/config.rs` +
    `qaqh-config/src/config.rs`，超出本批次允许改动范围）→ 已立为 **B-6**。
  - 未覆盖：`run_auto_compact` → `ContinueTurn` → 重发的完整回收链路无自动测试
    （`prepare_gate_snapshot` 需完整 `RingContext` + provider，须新建集成测试文件）。
  - 位置：`crates/qaqh-runtime/src/agent/engine_turn.rs:880-925`（唯一压缩触发路径）
  - 现状：`rg "CONTEXT_OVERFLOW|context_overflow" crates/` 零命中；`rg "context_limit" crates/qaqh-gate/src/` 零命中；`rg "context_window" crates/qaqh-config crates/qaqh-types` 只命中文档。触发条件只有 `decision_tokens > limit × threshold`，**无「连续 400 ⇒ 强制压缩重试」分支** ⇒ 上下文超限时 400 整轮 Fatal（E2）
  - 动作：先做「超限请求本地 pre-flight」——拿 `context_limit` 在发请求前预判并触发压缩；再做「端点声明上下文窗口」（profile schema 增 `context_window` 字段）
  - 验收：构造超限请求，期望**本地**触发压缩而非收到 400；`rg -n 'CONTEXT_OVERFLOW' crates/` 出现处理分支
  - 关联：D-15 / BUG-2026-09-16-04「待办」
  - ⚠️ 注意：清单记的「已把 `context_limit` 改成 240000」**已失效**——当前 `~/.config/qaqh/config.toml` 为 `context_limit = 1000000` + `auto_compact_threshold = 0.9`（阈值 900k）。此项若依赖该配置，需先确认阈值口径。

---

## 批次 6：热重载与 MCP 文案 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-runtime/src/service.rs`、`crates/qaqh-mcp/src/resources.rs`

- [x] **T-6-1 删除 reloader 的前置 `changed()` 守卫**（P1）→ b4851b0（PR #92）
  - ⚠️ **真机半条待主代理复测**：沙箱内起不了 daemon、改不了 `~/.config/qaqh/config.toml`。
    「重启 daemon 后第一次改 `[lsp]` 段 → 日志立即出现 `[lsp] hot-reload applied`」需在真实环境验证。
    可验证部分已由回归测试锁定（`first_publish_after_subscribe_is_observed` 红→绿）。
  - 位置：`crates/qaqh-runtime/src/service.rs:733-734`（mcp）、`:775-776`（lsp）；对照 `:736`、`:778` 是真正处理循环
  - 现状：`if rx.changed().await.is_ok() { rx.borrow_and_update(); }` 在真正循环之前。`qaqh_config::watch::subscribe()`（`crates/qaqh-config/src/watch.rs:34`）返回的 receiver 其 version 已是当前版本 ⇒ 那次 `changed()` 等到的是**用户启动后的第一次真实改动**，随即被 `borrow_and_update()` 丢弃 ⇒ 首次改 `[lsp]`/`[mcp]` 被静默吞掉（E2）
  - 动作：删掉两处前置守卫（`subscribe()` 语义下该守卫的意图不成立）；保留循环体内的 `if published.<sec> == manager.config() { continue; }` 幂等判定
  - 验收：重启 daemon 后**第一次**改 `config.toml` 的 `[lsp]` 段 → 日志立即出现 `[lsp] hot-reload applied`；新增回归测试覆盖「首次变更不被吞」
  - 关联：D-16① / BUG-2026-09-15-07（热重载那份）

- [x] **T-6-2 `list_resources` 区分「未连接」与「已连接但为空」**（P2）→ b4851b0（PR #92）
  - 遗留（未做，建议另开单）：`resources/list` **拉取失败**时缓存同样是 `None`，仍落「not connected yet」；
    三态拆分需 `connection.rs` 侧记录失败状态。
  - 位置：`crates/qaqh-mcp/src/resources.rs:181-191`
  - 现状：`resources.filter(|r| !r.is_empty())` 把 `Some(vec![])`（已连接、无资源）与 `None`（未连接）合并成同一分支，`:187-190` 输出 `"no resource list available — server not connected yet; …"`；同文件的 `list_prompts` 区分正确（E2）
  - 动作：拆成两个分支——`None` 保留未连接文案，`Some(empty)` 输出「已连接，资源列表为空」
  - 验收：对空资源 server 调 `list_resources` → 文案不含 "not connected"；`cargo test -p qaqh-mcp` 全绿
  - 关联：D-17 / BUG-2026-09-15-08

---

## 批次 7：零散修复 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-gate/src/chat_completions_api.rs`、`crates/qaqh-lsp/src/manager.rs`

- [x] **T-7-1 chat 路径补 `null→{}` 兜底**（P1）→ c627a1b（PR #94；组装抽成 `assemble_streamed_message` 以便单测，逻辑逐行等价。未做可选的 `name.is_empty()` 过滤——anthropic 侧也不做，加了反而制造两条路径不一致）
  - 位置：`crates/qaqh-gate/src/chat_completions_api.rs:695-697`；对照 `crates/qaqh-gate/src/message_api.rs:739-745`（anthropic 侧已有）
  - 现状：`serde_json::from_str(&args_json).unwrap_or(Value::Null)`，无 `null→{}` 兜底、无 `name.is_empty()` 过滤。而 `is_hanging_tool_use`（`crates/qaqh-types/src/message.rs:120-125`）只判 id/name 是否为空 ⇒ 流在「拿到 id+name、arguments 增量未到」时中断（`args_json == ""`）时 `input` 落成 `Null`，逃过持久化前清洗（`crates/qaqh-message/src/store.rs:798`）被写盘；出站序列化（`:823`）产生 `"arguments": "null"`，部分端点回 400（E2）
  - 动作：chat 的 Done 组装处对齐 anthropic——`if input.is_null() { json!({}) }`；若要更严格，对 `stop_reason.is_none()` 的抢救路径丢弃 args 解析失败的调用
  - 验收：新增单测 `chat_tool_use_null_input_becomes_empty_object`
  - 关联：D-18 / BUG-2026-09-13-13（清单需同步改为 `PARTIAL`）

- [x] **T-7-2 LSP 连接表随空闲回收摘除**（P1）→ c627a1b（PR #94；**选惰性摘除**——回收看门狗在 `connection.rs` 内，同步摘除需改连接对象所有权（违反本批次文件约束）；惰性摘除挂在 `get_or_connect` 入口，而表增长只由该入口触发，故天然有界。判据 `Disconnected` **且** `Arc::strong_count == 1`，排除建连窗口与冷却中/在用）
  - 位置：`crates/qaqh-lsp/src/manager.rs:202-215`（`get_or_connect` 插入）；HEAD 上仅 `:141`（配置变更）、`:309`（`shutdown_all`）两处 remove
  - 现状：按 `(server, root)` 插入连接后**从不摘除**；连接对象自身会被 `idle_shutdown_secs` 回收，但 map 条目保留 ⇒ 长驻 daemon 跨项目使用时缓慢增长（E2）
  - 动作：连接空闲回收时同步摘除 map 条目（路由键生命周期与连接生命周期绑定），或改惰性摘除
  - 验收：新增单测断言空闲回收后 `conns.len()` 回落
  - 关联：D-19

---

## 批次 8：安全 P1/P2 收尾 ← 建议在批次 2 之后

- 依赖：批次 2（同一批文件的后续改动，避免冲突）
- 涉及：`crates/qaqh-config/src/config.rs`、`crates/qaqh-mcp/src/connection.rs`、`crates/qaqh-workspace/src/file_state.rs`、`tools/cnb-mcp-enhance/auth.mjs`、`crates/qaqh-workspace/src/audit.rs`、`tools/cnb-mcp-enhance/server.mjs`

- [x] **T-8-1 MCP 并发上限收紧 + DynamicTool 权限层**（P1）→ 440a608（PR #95）
  - 配置校验 `1..=64` → `1..=16`，并在 `connection.rs` 执行点加 `min(16)` 运行时兜底；
    `mcp__` 前缀的 D5 快路径不再无条件放行——**Exec/Net** 类在 Level 1/2/3 强制 `AskUser`。
  - 一并更新两条与旧行为冲突的既有断言（`rejects_concurrency_out_of_range`、
    `mcp_tools_bypass_approval_at_all_levels` → `mcp_exec_net_require_approval_until_unrestricted`）。
  - ⚠️ **边界**：默认 `permission_level = 4`，Level 4 对内置 exec/网络工具同样全放行 ⇒
    本次消除的是「MCP 工具**独有**的 allow-all 特权」，不是「任何档位都弹审批」。
  - 位置：`crates/qaqh-config/src/config.rs:293-302`（校验 `1..=64`）、`crates/qaqh-mcp/src/connection.rs:717`（执行点）
  - 现状：上限是 64 而非 16；`DynamicTool` 权限层默认 allow-all（E2）
  - 动作：`max_concurrent_calls` 收紧到 `<= 16`；`DynamicTool` 对 `Exec`/`Net` 默认强制 `Permissions::AskUser`
  - 验收：集成测试 `mcp_concurrency_ceiling_16`、`mcp_dynamic_tool_requires_permission`
  - 关联：O-4 / 安全审查 P1-1

- [x] **T-8-2 CNB `auth.mjs` token 原子写**（P1）→ 440a608（PR #95）
  - 改为**同目录** tmp + `rename`（跨文件系统 rename 不保证原子）；tmp 名带 pid/时间戳/nonce。
  - 验证：并发 48 路 refresh + 紧循环读者、2 MiB payload ⇒ 修复版 `PASS`，
    旧版稳定 `FAIL … 9-10 partial/truncated reads`。
  - 位置：`tools/cnb-mcp-enhance/auth.mjs:86-87`
  - 现状：`await mkdir(...)` + `await writeFile(TOKEN_FILE, ...)` 直写，非原子 ⇒ 并发刷新可产生 partial write / token 丢失（E2）
  - 动作：改为 `tmp` + `rename`（与 `crates/qaqh-config/src/secrets.rs:336-346` 的 `next_temp_path` + `write_doc` 同种机制）
  - 验收：lint 测试 `auth_refresh_token_atomic`；模拟并发刷新 → token 不丢
  - 关联：安全审查 P1-2

- [x] **T-8-3 审计与账本键收尾**（P2）→ 440a608（PR #95）
  - ① `audit.csv` 4 MiB 上限 + 3 代 rotate（只 `rename` 不 `truncate`，rename 失败宁可继续 append
    不丢记录；进程内 mutex 串行化「判定 + append」）。上限值/代数由实现选定（清单未指定）。
  - ② **保守方案**：`resolve_workspace_path` 只做词法归一，账本层 `file_state::state_key`
    再做 best-effort `canonicalize`（剥 Windows `\\?\` 前缀）。不把 canonicalize 放进共享 resolve——
    会与既有键形态分叉（BUG-2026-09-13-16 的根因），且相对键是账本既有契约。
  - ③ `repoPath` 加 `^[A-Za-z0-9_./-]+$` 校验，并把裸拼的 `getBuildStage` 收编进 `repoPath`。
  - ④ `state_reason` 改显式参数（默认 `completed`）+ `{completed, not_planned}` 枚举校验。
  - ⚠️ 执行期间有一次**意外的线上探针调用**：`repo:"evil"` 通过白名单发出真实
    `PATCH https://api.cnb.cool/evil/-/issues/1` → 404（仓库不存在，无副作用）。
  - 位置：`crates/qaqh-workspace/src/audit.rs:31-59`（无 rotation）、`crates/qaqh-workspace/src/file_state.rs:128-158` + `crates/qaqh-workspace/src/lib.rs:424-441`（键未 canonicalize）、`tools/cnb-mcp-enhance/server.mjs:87`（`repoPath` 无校验）、`:299/:438`（`state_reason` 硬编码）
  - 现状：`audit.csv` 只 append、无大小上限；`resolve_workspace_path` 对绝对路径原样返回不做归一（`lib.rs:430-432`）+ 符号链接不解析 ⇒ 账本键可能分歧；`repoPath = (repo) => \`/${repo || DEFAULT_REPO}\`` 无 `^[A-Za-z0-9_./-]+$` 校验；`cnb_issue_close` 硬编码 `state_reason=completed`（E2）
  - 动作：① `audit.csv` 加大小上限 + rotate；② 账本键改为 `canonicalize(resolve_workspace_path(raw))`（注意 Windows 大小写与符号链接语义）；③ `repoPath` 加字符白名单校验；④ `state_reason` 改为显式参数
  - 验收：单测 `audit_csv_growth_bounded`、`file_state_key_matches_resolved_key`；`rg -n 'state_reason' tools/cnb-mcp-enhance/server.mjs` 出现参数来源
  - 关联：O-5 / 安全审查 P1-3、P2 表

---

## 批次 9：清单归档与卫生（文档，可独立下发）← 不涉及代码

- 依赖：无
- 涉及：`docs/buglist/*`

- [x] **T-9-1 归档 5 条状态过期条目** → 563f1e3（PR #91，已核实 `ea6063c`/`d9fa81c` 均为 HEAD 祖先）
  - 现状：`2026-09-14-timeline工具块内存放大-buglist.md` 的 `BUG-2026-09-14-01` ~ `-04` 仍写 `fixed（工作区，待提交）`，实际已随 `ea6063c`（2026-09-15）提交且是 HEAD 祖先；`2026-09-15-热重载吞首次变更与资源文案-buglist.md:17` 的 `BUG-2026-09-15-09` 同样，实际已随 `d9fa81c` 提交
  - 动作：状态改为 `fixed @ea6063c` / `fixed @d9fa81c`；同文件的「修复优先级 → 已完成（工作区，待提交）」小标题一并更新；表头「复核基线 `1c92413`」注明那只是登记提交
  - 验收：`rg -n '工作区，待提交' docs/buglist/` → 不再命中已提交项
  - 关联：D-20

- [x] **T-9-2 修正 `BUG-2026-09-17-06` 的描述** → 563f1e3（PR #91，标「HEAD 不成立，待确认运行二进制版本」+ TUI `33253a5` 证据）
  - 现状：该条称 TUI `apply_status` 收到 `CANCELLED` 后不停跟踪；实测 TUI 仓 `~/Projects/qaqh-tui-app` @ `33253a5` 的 `src/app/mod.rs:844-856` 已调用 `untrack_subagent`（定义 `src/app/subagent.rs:352-356`，接线来自 `f85c33d`，2026-09-09 即已在历史里）
  - 动作：标注「HEAD 不成立；日志证据与代码事实不一致，需先确认运行的是哪个版本的二进制」，或直接关闭
  - 验收：该条状态不再是裸 `open`
  - 关联：D-21

- [x] **T-9-3 修正 `BUG-2026-09-13-13` 的状态** → 563f1e3（PR #91，改为 `⚠️ PARTIAL @3775a9c`，残留指向 `chat_completions_api.rs:695-697`）
  - 现状：标 `✅ fixed @3775a9c`，但 chat 路径 `crates/qaqh-gate/src/chat_completions_api.rs:695-697` 仍缺 `null→{}` 兜底与 name 过滤 ⇒ 应改为 `PARTIAL`（或拆出新条目）
  - 动作：改状态 + 注明残留子情形与对应位置
  - 验收：状态列含 `PARTIAL` 或新条目已登记
  - 关联：D-18

- [x] **T-9-4 清单卫生四项** → 563f1e3（PR #91）
  - ① 撞号消歧：keepalive 那份改 `keepalive-BUG-2026-09-15-07`（加文件前缀，保留子串可查性）；
    ② 回填哈希按索引表纠正；③ 示例伪表行改等长占位号；④ timeline 两处标「已由 `ea6063c` 处理」。
  - 现状：① `BUG-2026-09-15-07` 被 `keepalive` 与 `热重载` 两份文件同时占用；`BUG-2026-09-16-01` 被 `read_image` 与 `edit` 同时占用；`BUG-2026-09-13-31` 与 `-08` 是同一缺陷重复登记。② `2026-09-12-多会话…` 的「状态回填」注记把 09/10/11/12 写成 `90d7051 / fbb7f4d / 4197db1 / 86264b7`（那四个其实是 09-13 清单的 `Closes #9/#10/#11/#12`），与同文件索引表的 `33e6261 / 13cb21e / 13cb21e / fe4da88` 矛盾。③ `2026-09-16-edit工具行内片段与kind虚报-buglist.md` 的复现示例在代码块内内嵌了一行伪造的 `| BUG-2026-09-16-01 | fixed … |`，任何 `^\| BUG-` 的 grep 都会误命中。④ `2026-09-12-timeline死锁…:25-26` 与 `2026-09-12-timeline快照…:30` 仍称 `enable_turn_offload` 是死代码、ABBA 锁序未改，与 `ea6063c` 之后的代码事实相反
  - 动作：① 重编 ID 或加文件前缀；② 修正哈希；③ 改掉示例里的 ID；④ 标注「已由 `ea6063c` 处理」
  - 验收：`rg -n '^\| BUG-2026-09-15-07' docs/buglist/` → 单份文件命中
  - 关联：报告 §附录 B 第 6 组

---

## 阻塞项（需现场环境，**不要派 codex-cli**）

- [ ] **B-1** `exec` 管道文件的 7 条未闭环项（`H1a` spawn 期 stdio 接线异常、`H1b` 写端泄漏/永不关闭、`§C` daemon live 取证、`§E.2` 写端持有者枚举、`§E.3` 失败现场直捕、`§H` 遗留待复测、`§A.2` 关联观察）
  - 缺什么：Windows 11 + 安装版 daemon + `%TEMP%\qaqh-exec-probe\` 工具集；`pipe_scan2.rs` 需先做超时化改造
  - 已确认：`30a011b` 把「Peek 失败即 break」改为排空到 `Ok(0)`，这些观测**不会**再表现为旧的「零字节 + `truncated:true`」
- [ ] **B-2** 09-14 清单的 O-2（TUI 侧非 bash progress 归一/限长，声称随 `59541dd` 修复）
  - 缺什么：TUI 仓可读访问；`59541dd` 在本仓不存在
- [ ] **B-3** 性能收益复测（08 的 72k→110k ev/s、09 的 2 MiB→18.9 ms、12 的 62–247 ms、13 的 O(n) 阶跃、14 的吞吐、09-14 的 63 ms→0.01 ms）
  - 缺什么：可跑基准的环境（本轮只确认结构性改动在位）
- [ ] **B-4** `2026-09-16-安全并发与审查登记` 的 P2/P3 表剩余行
  - 缺什么：`ACTOR_WORKSPACE` 那条已被 `crates/qaqh-workspace/src/runtime.rs:167-176` 的 `ActorToolScope` 部分缓解，**需重判**；另两行清单行号已失效，需重定位
- [ ] **B-5** D-2 的审批面板可见性（Level 3 弹窗里用户能否看到 `exec` 的目标路径）
  - 缺什么：实机点击验证（本轮是从 `extract_target_paths` 对 `exec` 参数名的行为推断的）

---

## 后续待办（执行批次过程中新发现，非阻塞，尚未派发）

> 这些不在原复核范围内，是子代理执行/评审时暴露出来的。**未派发**，待机主决定优先级。

- [ ] 🔴 **N-5（P0，安全，来自 T-2-2 的未闭合半条）`exec` 在 Level 4 可写工区外路径**
  - 现状：`is_path_in_workspace` 只能从 `ctx.args["path"]` 判定目标，而 `exec` 的 schema 里
    **没有 `path`**（只有 `command`/`argv`/`cwd`）⇒ 无法从 `command` 文本判定写入目标；
    Level 4 完全自动批准 ⇒ **`exec` 仍可写工区外路径**（安全审查 P0-2 的原始攻击面）。
  - 为什么没在批次 2 顺手修：`exec` 也是 `ToolRisk::Destructive`，字面 fail-closed 会
    在所有权限等级阻断它（实测打红两条既有测试）；真正的收口有两条路，**都需要机主定产品口径**：
    - (a) 权限层收口：`needs_permission`/`authorize_call` 里 Level 4 **不再无条件放行**
      Exec/Net（会新增审批弹窗，改变日常使用体感）；
    - (b) 给 `exec` 加沙箱（工作区外的写入在系统调用层被拒，改动面更大）。
  - 关联：安全审查 P0-2 / D-2 / PR #93 描述

- [ ] **N-1（来自 T-5-2 的未完成半条）profile schema 增 `context_window` 字段**
  - 位置：`crates/qaqh-types/src/config.rs`、`crates/qaqh-config/src/config.rs`
  - 现状：批次 5 只做了本地 pre-flight（以既有 `context_limit` 当硬窗口），
    「端点声明上下文窗口」这半条因超出批次 5 允许改动的文件范围而未做。
  - 动作：profile schema 增 `context_window`，pre-flight 优先用它、缺失时回落 `context_limit`。
  - 关联：D-15 / BUG-2026-09-16-04

- [ ] **N-2（来自 PR #87 自动评审的建议项）`apply_patch` 三处收尾**
  - ① `WOULD_OVERWRITE` 的 hint 措辞在「真 apply 走到这里」时不成立（前序 hunk 已落盘，
    重发同 patch 会 `NO_MATCH`）⇒ 需区分 dry_run / 非 dry_run 两种场景。
  - ② `file_state` 未随 `apply_patch` 的覆盖同步（与既有 `UpdateFile`/`Move` 分支一致，
    但需确认紧随其后的 `edit` 是否会被 hash 漂移拒绝）。
  - ③ `OVERWROTE` 文本只取首个 `*** Add File:` marker，多 Add File 时会漏报。
  - 关联：PR #87 评审评论 / BUG-2026-09-16-10、-11

- [ ] **N-3（来自批次 6 的附带观察）`list_resources` 的三态拆分**
  - 现状：`resources/list` **拉取失败**与**未连接**目前都落到「not connected yet」文案；
    批次 6 只拆了「已连接但为空」。
  - 动作：`connection.rs` 侧记录失败状态，`resources.rs` 输出第三种文案。
  - 关联：D-17 / BUG-2026-09-15-08

- [ ] **N-4（来自 PR #87 评审的解析类观察）`apply_patch` 解析边界**
  - ① `*** End Patch` 被当作 Add File 目标（`strip_prefix` 未校验 rest 为空）⇒ 返回 Ok 并真的建文件；
    控制用例（只有 END marker）现在也返回 Ok。
  - ② 文件名为空时 `resolve_workspace_path` 的 `ParentDir` 分支会 `pop()` 掉 workspace 根。
  - ③ 符号链接 workspace + 绝对路径：`resolve_workspace_path` 的 `joined.exists()` 走 canonicalize
    与原 cwd 比对，会拒掉合法请求。
  - 说明：三条**早于批次 3 存在**，与批次 3 的改动无因果关系。
  - 关联：PR #87 评审评论附注

---

## 下发模板（复制即用）

```bash
/home/qaqtamsy/Desktop/ws/codex exec -s workspace-write \
  -C /home/qaqtamsy/Projects/qaqh-backend \
  -o /tmp/batch-N-report.md \
  "你在 /home/qaqtamsy/Projects/qaqh-backend 工作。基线 commit 661e5f4。

   执行 docs/todo/2026-09-17-buglist复核-checklist.md 的【批次 N】。
   规则：
   1. 只做该批次列出的条目，不要顺手改别的东西。
   2. 每条按其「动作」实现最小改动，然后跑它给出的「验收」命令并原样粘贴输出。
   3. 验收失败的不要硬说通过；写清失败原因与你的判断。
   4. 完成后把该批次条目的 [ ] 改成 [x] 并补 → {commit} 或 → {验收命令原文}，
      同时把「进度总览」表里该批次的状态改为 ☑。
   5. 不要修改 docs/buglist/ 下任何文件（那是批次 9 的事）。"
```
