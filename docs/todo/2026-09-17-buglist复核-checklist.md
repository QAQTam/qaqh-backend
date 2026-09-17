# buglist 复核 — 执行清单（2026-09-17）

> 来源：`./2026-09-17-buglist复核-report.md`
> 基线：`661e5f4`；工作树 `crates/` 零改动。所有行号以该 commit 为准。
> 用法：**一个批次 = 一次 codex-cli 下发**。批次内条目按序执行；批次之间互不依赖，可并行。
> 二进制：`/home/qaqtamsy/Desktop/ws/codex`（**只用这个路径**，其余是旧版实现）。
> 勾选规则：`[x]` 之后必须补 `→ {commit}` 或 `→ {验收命令原文}`，禁止空勾。

## 进度总览

| 批次 | 主题 | 条目数 | 严重度 | 依赖 | 状态 |
|---|---|---|---|---|---|
| 1 | 子代理取消链 | 5 | P0/P1/P2 | 无 | ☐ |
| 2 | 安全 P0（越权/边界/凭据/泄漏） | 4 | P0 | 无 | ☐ |
| 3 | `apply_patch` 契约 | 3 | P0/P1 | 无 | ☐ |
| 4 | `edit` 契约 | 3 | P0/P1 | 无 | ☐ |
| 5 | 计量与超限 | 2 | P1 | 无 | ☐ |
| 6 | 热重载与 MCP 文案 | 2 | P1/P2 | 无 | ☐ |
| 7 | 零散修复 | 2 | P1 | 无 | ☐ |
| 8 | 安全 P1/P2 收尾 | 3 | P1/P2 | 批次 2 | ☐ |
| 9 | 清单归档与卫生（文档） | 4 | — | 无 | ☐ |
| — | 阻塞项（需现场环境，不派） | 8 | — | — | 🚫 |

---

## 批次 1：子代理取消链 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-runtime/src/registry.rs`、`crates/qaqh-subagent/src/lib.rs`、`crates/qaqh-runtime/src/agent/loop_injection.rs`、`crates/qaqh-runtime/src/agent/engine_input.rs`、`crates/qaqh-workspace/src/process_registry.rs`、`crates/qaqh-runtime/src/ringing/hub.rs`
- 说明：这批是机主报告的「已取消的子代理怎么复活了」的完整根因链。清单自述 T-1-1 是 1 行、T-1-2 是 1 个守卫。

- [ ] **T-1-1 子代理 spawn 时注册 liveness**（P0）
  - 位置：`crates/qaqh-runtime/src/registry.rs:361-430`（`spawn_subagent_inprocess`）
  - 现状：全文无 `hub.mark_worker_live(seed)`；`hub` 在 `:386` 被 clone 后只交给 event reader 线程。对照 `:292`（`get_or_spawn`）与 `:715`（`respawn_dead_agents`）都有。⇒ 子 seed 不在 `live_workers`，bootstrap 的 orphan seal 会把它判为孤儿封禁（E2）
  - 动作：在 `:425` 插入 `self.instances` 之后补 `self.hub.mark_worker_live(seed);`
  - 验收：`rg -n 'mark_worker_live' crates/qaqh-runtime/src/registry.rs` → 出现 3 处（含 `spawn_subagent_inprocess`）；新增单测 `spawn_subagent_registers_liveness`；daemon 日志出现 `worker alive for {seed}` 而非 `sealing orphan active turn`
  - 关联：D-5 / BUG-2026-09-17-01

- [ ] **T-1-2 取消后不再注入结果**（P0）
  - 位置：`crates/qaqh-subagent/src/lib.rs:533-595`
  - 现状：`did_cancel` 只在 `:523` 决定 `state_tag` 文案；`:543-586` 的注入块无守卫，取消后仍把完整 `final_answer` 注入父会话。`registry_ref.finish()` 在注入**之后**（`:597`）（E2）
  - 动作：`:533` 条件改为 `if !parent_seed.is_empty() && !did_cancel`；若产品上要留痕，改为注入**不含 `final_answer`** 的状态行
  - 验收：新增单测 `cancelled_collector_does_not_inject`；cancel 路径日志不再出现 `inject accepted`
  - 关联：D-6 / BUG-2026-09-17-02

- [ ] **T-1-3 系统注入区分「用户取消」与「回合取消」**（P1）
  - 位置：`crates/qaqh-runtime/src/agent/loop_injection.rs:196-220`、`crates/qaqh-runtime/src/agent/engine_input.rs:261-262`
  - 现状：`LoopPhase::Idle` 分支无条件 `handle_system_input` 开新回合；`engine_input.rs` 主动 `ctx.cancel.clear(); qaqh_workspace::clear_cancel();` ⇒ 任何系统注入都能复活已取消会话（E2）
  - 动作：引入取消原因位（`cancel_reason` / `user_cancelled`）；系统注入仅在**非用户取消**态清除标志并开回合
  - 验收：取消后父会话不再出现新的 `TurnStart`；单测覆盖「用户取消后系统注入被拒」
  - 关联：D-7 / BUG-2026-09-17-03

- [ ] **T-1-4 父会话取消传播到子 seed**（P1）
  - 位置：`crates/qaqh-runtime/src/registry.rs`（新增登记）、`crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs:95-124`（取消处理）
  - 现状：全仓 `rg -n 'children|descendants|child_seeds' crates/qaqh-runtime/src` 在 cancel 路径零命中；父会话没有「自身子 seed 集合」的登记能力（E2）
  - 动作：spawn 时登记 `parent_seed -> {child_seeds}`，`forget_seed` 时清理；cancel 处理遍历并逐个 `cancel` + `mark_worker_dead`
  - 验收：新增单测 `parent_cancel_propagates_to_children`；daemon 日志显示子 seed 收到取消
  - 关联：D-8 / BUG-2026-09-17-04

- [ ] **T-1-5 状态单调性守卫 + 接入 `mark_worker_dead`**（P2）
  - 位置：`crates/qaqh-workspace/src/process_registry.rs:478-486`、`crates/qaqh-runtime/src/ringing/hub.rs:752`
  - 现状：`mark_exited` 无条件 `= ProcStatus::Exited(code)`，会把 `Killed` 覆盖回 `Exited`，违反同文件 `:463-465` 注释自称的单调性；`mark_worker_dead` 全仓**零生产调用点**（仅 `:3417` 一个测试调 `mark_worker_live`），`live_workers` 只靠 `forget_seed`（`:791-793`）清理（E2）
  - 动作：`mark_exited` 加守卫——仅当前状态为 `Running` 时才改写；把 `mark_worker_dead` 接到子代理退出路径（与 T-1-1 同一处改动）
  - 验收：新增单测 `mark_exited_does_not_downgrade_killed`；`rg -n 'mark_worker_dead' crates/` 出现生产调用点
  - 关联：D-9、D-10 / BUG-2026-09-17-05、-07

---

## 批次 2：安全 P0 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-runtime/src/service/fs_git.rs`、`crates/qaqh-runtime/src/service.rs`、`crates/qaqh-workspace/src/manager.rs`、`crates/qaqh-workspace/src/safety.rs`、`crates/qaqh-daemon/src/axum_server.rs`、`crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs`、`crates/qaqh-session/src/manager.rs`

- [ ] **T-2-1 `fs.read`/`fs.list` 加路径白名单**（P0）
  - 位置：`crates/qaqh-runtime/src/service/fs_git.rs:10-64`、`:68-91`；入口 `crates/qaqh-runtime/src/service.rs:232-243`
  - 现状：两个函数只校验 `is_absolute()`，源码注释 `:8-9` 自述「临时跨端版本有意不做路径沙箱/权限校验」⇒ 持 token 者可读列任意绝对路径（E2）
  - 动作：两入口各加 `allowed_roots(workspace_root, data_dir)` 前缀校验（组件级比较，复用 `crates/qaqh-workspace/src/permission.rs:375` 的 `path_within_dir`）；拒绝返回 `FORBIDDEN` 而非 `IO_ERROR`；显式复用 `is_sensitive_session_path` 取代按 `meta.json` 子串过滤；删除 `:8-9` 的过时注释
  - 验收：新增集成测试 `fs_read_rejects_meta_json`、`fs_list_rejects_sessions_dir`，断言返回码非 IO；`cargo test -p qaqh-runtime` 全绿
  - 关联：D-1 / 安全审查 P0-1

- [ ] **T-2-2 Destructive 工具缺 `path` 参数时 fail-closed**（P0）
  - 位置：`crates/qaqh-workspace/src/manager.rs:525-549`（`is_path_in_workspace`）、`crates/qaqh-workspace/src/safety.rs:14-24`
  - 现状：`is_path_in_workspace` 只读 `ctx.args["path"]`；`exec` 的参数名是 `command`，于是走 `:545-548` 的 `else` 分支**恒返回 `true`**。`exec` 在 `exec/register.rs:29` 声明为 `ToolRisk::Destructive`，而 `safety.rs:18-21` 只在 `(Destructive, false)` 时阻断 ⇒ 判定被短路，永远放行。Level 4（`permission.rs:489-491`）完全自动批准（E2）
  - 动作：`is_path_in_workspace` 的 `else` 分支按 risk 分级——`Destructive` 工具缺 `path` 时返回 `false`（fail-closed）；`Write`/`ReadOnly` 维持 `true` 以免误伤 `task`/`skills`/`ask`
  - 验收：新增单测 `destructive_tool_without_path_is_treated_as_outside_workspace`；e2e 覆盖 Level 4 下 `exec` 写工区外路径 → 期望被 `SafetyPolicy` 阻断
  - 关联：D-2 / 安全审查 P0-2

- [ ] **T-2-3 停止泄露 token**（P0）
  - 位置：`crates/qaqh-daemon/src/axum_server.rs:14`、`crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs:105-106`
  - 现状：`/health` 返回 `format!("ok epoch={} token_len={}", …)`；debug 桥把明文 `state.token` 注入 `window.__QAQH_DEBUG__`。回环 + Host 双检（`debug_control.rs:283`、`:306`）已在，但 nonce 一次性兑换 / `Sec-Fetch-Site` / 常量时间比较三项加固未做（E2）
  - 动作：`/health` 改为只报 `ok epoch={}`；桥脚本改为**只下发 nonce**，token 由客户端凭 nonce 走一次兑换接口换取、兑换后作废
  - 验收：单测断言 `/health` 响应体不含 `token` 子串；`/debug` 响应体不含真实 token 字面量
  - 关联：D-3 / 安全审查 P0-3

- [ ] **T-2-4 `session_locks` 在删除路径释放**（P0）
  - 位置：`crates/qaqh-session/src/manager.rs:1380-1385`（insert）、`:223-241`（`delete()`）
  - 现状：`delete()` 只做 `invalidate_watermark` / `remove_dir_all` / `remove_from_index` / `remove_session`，**不碰 `session_locks`**；`:1237` 的 `release_seed_claim` 清的是另一个 map（`claimed_seeds`）⇒ 每删一个会话永久多留一条（E2）
  - 动作：`delete()` 末尾加 `self.session_locks.lock()?.remove(seed);`，并同步 `WorkspaceStore::remove_session` 侧；注意与 `session_lock()` 的持锁顺序，避免反向获取
  - 验收：新增单测 `session_locks_shrinks_after_delete`（删前/删后 `len()` 相等）；`cargo test -p qaqh-session` 全绿
  - 关联：D-4 / 安全审查 P0-4

---

## 批次 3：`apply_patch` 契约 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-workspace/src/apply_patch.rs`、`crates/qaqh-workspace/src/apply_patch_engine/mod.rs`、`crates/qaqh-workspace/src/apply_patch_engine/seek_sequence.rs`

- [ ] **T-3-1 `Add File` 对已存在路径加守卫**（P0）
  - 位置：`crates/qaqh-workspace/src/apply_patch_engine/mod.rs:185-186`（真 apply）、`:305-317`（dry-run）、`:188-193`（delta `old: None`）
  - 现状：`Hunk::AddFile` 直接 `write_file_with_missing_parent_retry`，无 exists 检查；dry-run 也只拒绝目录。⇒ `dry_run=true` 返回普通 `[DRY RUN] … ok`，真 apply 整文件覆盖且返回体不含 overwrite 字段（**静默数据丢失**）（E2）
  - 动作：`AddFile` 加 exists 守卫；`dry_run` 对已存在路径返回 `WOULD_OVERWRITE`；把旧内容读进 `FileDelta.old` 以便回滚
  - 验收：新增单测 `add_file_refuses_existing_path`；手动复现清单探针（对已存在 5 行文件发 `*** Add File:`）→ 期望拒绝或显式 `WOULD_OVERWRITE`
  - 关联：D-11② / BUG-2026-09-16-10

- [ ] **T-3-2 失败 hint 改为陈述事实**（P1）
  - 位置：`crates/qaqh-workspace/src/apply_patch.rs:157`；对照 `apply_patch_engine/mod.rs:181`（`for hunk in &hunks`）+ `:273`（循环体内 `std::fs::write`）
  - 现状：hint 写「Re-send the FULL corrected patch — no partial application happened」，而引擎是逐 hunk 边算边写、非原子；同文件 `:7-9` 的模块文档自述的恰恰是「已写入的文件保留」，两处互相矛盾。模型按提示重发完整 patch 必然二次 `NO_MATCH`（E2）
  - 动作：hint 改为陈述事实（「已生效：a.txt；未生效：b.txt — 修正后**只重发失败部分**，或先 `git diff`/`read` 核对已生效文件」）
  - 验收：新增单测 `failed_hunk_hint_reports_partial_application`；手动复现清单 BUG-09 探针（1 号 hunk 合法、2 号上下文不存在）→ 期望文案列出已生效文件
  - 关联：D-11① / BUG-2026-09-16-09

- [ ] **T-3-3 补「上下文充分性」警示**（P2）
  - 位置：`crates/qaqh-workspace/src/apply_patch_engine/seek_sequence.rs:39-43`；工具描述 `crates/qaqh-workspace/src/apply_patch.rs:188-197`
  - 现状：exact 循环直接 `return Some(i)`，无歧义拒绝（对比 `edit` 会报 `Ambiguous`）；工具描述与格式说明都**没有**「同一上下文多处出现时必须补足上下文或用 `@@` 锚定」这条警示 ⇒ 静默改第一处并返回 `[OK]`（E2）
  - 动作：在工具描述里补该警示（保持「取首个命中」的现有语义不变，只补文案）；若决定改为歧义拒绝，需单独评估与上游 `codex-rs/apply-patch` 的行为差异
  - 验收：`sed -n '188,200p' crates/qaqh-workspace/src/apply_patch.rs` → 文案含歧义要求
  - 关联：D-11③ / BUG-2026-09-16-11

---

## 批次 4：`edit` 契约 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-workspace/src/edit/matching.rs`、`edit/resolve.rs`、`edit/mod.rs`、`edit/handler.rs`、`edit/transaction.rs`

- [ ] **T-4-1 描述与 schema 收敛到 3 个 kind**（P1，纯文案）
  - 位置：`crates/qaqh-workspace/src/edit/handler.rs:344`（工具描述）、`:356`（hunks schema）；`edit/mod.rs:10`（模块文档）；`edit/transaction.rs:341`（`INVALID_REGEX → replace_inline` 映射）
  - 现状：四处仍列 `insert_after` / `insert_before` / `replace_inline`，实现已在 `a92626d`（2026-09-08）删除 ⇒ 任何按描述发起的调用恒 `PARSE_ERROR: unknown hunk kind '…' (expected replace / prepend_file / append_file)`（E2）
  - 动作：描述与 schema 收敛到 `replace` / `prepend_file` / `append_file`；`transaction.rs:341` 的 `INVALID_REGEX` 提示改为指向 bash/python 做正则替换
  - 验收：`rg -n 'insert_after|insert_before|replace_inline' crates/qaqh-workspace/src/edit/` → 仅剩 `tests.rs` 的历史注释（或无）；新增用例断言按描述发起的调用不再出现在描述里
  - 关联：D-13 / BUG-2026-09-16-08

- [ ] **T-4-2 Tier3 采纳后替换区间锚定到 `old` 的行内 span**（P0）
  - 位置：`crates/qaqh-workspace/src/edit/resolve.rs:34-35`（`char_starts[start_line]..char_starts[start_line + win_lines]`）、`edit/matching.rs:102`（`TextDiff::from_chars().ratio()`）、`edit/mod.rs:32`（`T3_THRESHOLD = 0.85`）
  - 现状：字符级评分 ≥ 0.85 即采纳，但替换区间是**整个命中窗口**，不是 `old` 在行内的位置 ⇒ 行内片段 `old` 被采纳时，该行未被 `old` 覆盖的前后缀**无提示删除**，返回仍是 `1/1 hunks applied … score 0.98`（**静默丢内容**）（E2）
  - 动作：二选一——(a) 让 Tier3 的替换区间锚定到 `old` 的实际行内 span；(b) 采纳前要求 `old` 覆盖整行（否则不采纳）。方案 (b) 改动更小、语义更保守，建议优先
  - 验收：新增「片段 old」用例（清单自述 42 个 `#[test]` 里目前**一个都没有**）；`cargo test -p qaqh-workspace edit` 全绿；手动复现清单探针 → 期望不再出现前后缀被删
  - 关联：D-12① / BUG-2026-09-16-06

- [ ] **T-4-3 补第四种失败诊断「`old` 是行内片段」**（P1）
  - 位置：`crates/qaqh-workspace/src/edit/matching.rs:190-204`（`no_match_detail`）
  - 现状：只有「差阈值 / 差 margin / 完全不像」三种口径。当 `old` 是行内片段且达不到 0.85 时，报「best score 0.32 is below threshold 0.85 — closest location is probably wrong; re-check 'old' against the file」，而 `old` **逐字符就在候选行里** ⇒ 模型据此判定「自己记错了内容」，真因是「片段 vs 整行」（E2）
  - 动作：新增第四种诊断——检测到候选行**包含** `old` 子串时，提示「`old` 是行内片段，需给出完整行内容」
  - 验收：新增用例覆盖该诊断分支；手动复现清单探针 → 文案指向「片段 vs 整行」
  - 关联：D-12② / BUG-2026-09-16-07

---

## 批次 5：计量与超限 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-runtime/src/agent/state/token_calibration.rs`、`crates/qaqh-runtime/src/agent/engine_compact.rs`、`crates/qaqh-runtime/src/agent/engine_turn.rs`

- [ ] **T-5-1 图片字节不计入 token 估算**（P1）
  - 位置：`crates/qaqh-runtime/src/agent/state/token_calibration.rs:163-164`；同一盲点第二处 `crates/qaqh-runtime/src/agent/engine_compact.rs:432-435`
  - 现状：把整份 `(messages, tools)` `serde_json::to_string` 后交 `count_tokens`，而 `ToolResult.images[].data` 的内联 base64（`crates/qaqh-types/src/tool_result.rs:83-96`）就在这份字符串里，计数实现（`crates/qaqh-types/src/token.rs:19-28`）无图片感知 ⇒ 1 MiB 截图被折算成二三十万 token，而端点按像素只算几千；带图会话每轮触发 auto-compact，压缩后仍超阈值，prompt cache 反复失效（E2）
  - 动作：序列化前把图片字节替换为等价计费占位符（保守固定上限，改动最小、无契约变更）；`engine_compact.rs:432` 一并处理
  - 验收：新增回归锁断言 `prepared_request_metrics` 不随图片字节线性增长（当前必然失败）；`rg -n 'image|base64' crates/qaqh-runtime/src/agent/state/token_calibration.rs` 出现处理分支
  - 关联：D-14 / BUG-2026-09-16-05

- [ ] **T-5-2 超限请求本地 pre-flight**（P1）
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

- [ ] **T-6-1 删除 reloader 的前置 `changed()` 守卫**（P1）
  - 位置：`crates/qaqh-runtime/src/service.rs:733-734`（mcp）、`:775-776`（lsp）；对照 `:736`、`:778` 是真正处理循环
  - 现状：`if rx.changed().await.is_ok() { rx.borrow_and_update(); }` 在真正循环之前。`qaqh_config::watch::subscribe()`（`crates/qaqh-config/src/watch.rs:34`）返回的 receiver 其 version 已是当前版本 ⇒ 那次 `changed()` 等到的是**用户启动后的第一次真实改动**，随即被 `borrow_and_update()` 丢弃 ⇒ 首次改 `[lsp]`/`[mcp]` 被静默吞掉（E2）
  - 动作：删掉两处前置守卫（`subscribe()` 语义下该守卫的意图不成立）；保留循环体内的 `if published.<sec> == manager.config() { continue; }` 幂等判定
  - 验收：重启 daemon 后**第一次**改 `config.toml` 的 `[lsp]` 段 → 日志立即出现 `[lsp] hot-reload applied`；新增回归测试覆盖「首次变更不被吞」
  - 关联：D-16① / BUG-2026-09-15-07（热重载那份）

- [ ] **T-6-2 `list_resources` 区分「未连接」与「已连接但为空」**（P2）
  - 位置：`crates/qaqh-mcp/src/resources.rs:181-191`
  - 现状：`resources.filter(|r| !r.is_empty())` 把 `Some(vec![])`（已连接、无资源）与 `None`（未连接）合并成同一分支，`:187-190` 输出 `"no resource list available — server not connected yet; …"`；同文件的 `list_prompts` 区分正确（E2）
  - 动作：拆成两个分支——`None` 保留未连接文案，`Some(empty)` 输出「已连接，资源列表为空」
  - 验收：对空资源 server 调 `list_resources` → 文案不含 "not connected"；`cargo test -p qaqh-mcp` 全绿
  - 关联：D-17 / BUG-2026-09-15-08

---

## 批次 7：零散修复 ← 可独立下发

- 依赖：无
- 涉及：`crates/qaqh-gate/src/chat_completions_api.rs`、`crates/qaqh-lsp/src/manager.rs`

- [ ] **T-7-1 chat 路径补 `null→{}` 兜底**（P1）
  - 位置：`crates/qaqh-gate/src/chat_completions_api.rs:695-697`；对照 `crates/qaqh-gate/src/message_api.rs:739-745`（anthropic 侧已有）
  - 现状：`serde_json::from_str(&args_json).unwrap_or(Value::Null)`，无 `null→{}` 兜底、无 `name.is_empty()` 过滤。而 `is_hanging_tool_use`（`crates/qaqh-types/src/message.rs:120-125`）只判 id/name 是否为空 ⇒ 流在「拿到 id+name、arguments 增量未到」时中断（`args_json == ""`）时 `input` 落成 `Null`，逃过持久化前清洗（`crates/qaqh-message/src/store.rs:798`）被写盘；出站序列化（`:823`）产生 `"arguments": "null"`，部分端点回 400（E2）
  - 动作：chat 的 Done 组装处对齐 anthropic——`if input.is_null() { json!({}) }`；若要更严格，对 `stop_reason.is_none()` 的抢救路径丢弃 args 解析失败的调用
  - 验收：新增单测 `chat_tool_use_null_input_becomes_empty_object`
  - 关联：D-18 / BUG-2026-09-13-13（清单需同步改为 `PARTIAL`）

- [ ] **T-7-2 LSP 连接表随空闲回收摘除**（P1）
  - 位置：`crates/qaqh-lsp/src/manager.rs:202-215`（`get_or_connect` 插入）；HEAD 上仅 `:141`（配置变更）、`:309`（`shutdown_all`）两处 remove
  - 现状：按 `(server, root)` 插入连接后**从不摘除**；连接对象自身会被 `idle_shutdown_secs` 回收，但 map 条目保留 ⇒ 长驻 daemon 跨项目使用时缓慢增长（E2）
  - 动作：连接空闲回收时同步摘除 map 条目（路由键生命周期与连接生命周期绑定），或改惰性摘除
  - 验收：新增单测断言空闲回收后 `conns.len()` 回落
  - 关联：D-19

---

## 批次 8：安全 P1/P2 收尾 ← 建议在批次 2 之后

- 依赖：批次 2（同一批文件的后续改动，避免冲突）
- 涉及：`crates/qaqh-config/src/config.rs`、`crates/qaqh-mcp/src/connection.rs`、`crates/qaqh-workspace/src/file_state.rs`、`tools/cnb-mcp-enhance/auth.mjs`、`crates/qaqh-workspace/src/audit.rs`、`tools/cnb-mcp-enhance/server.mjs`

- [ ] **T-8-1 MCP 并发上限收紧 + DynamicTool 权限层**（P1）
  - 位置：`crates/qaqh-config/src/config.rs:293-302`（校验 `1..=64`）、`crates/qaqh-mcp/src/connection.rs:717`（执行点）
  - 现状：上限是 64 而非 16；`DynamicTool` 权限层默认 allow-all（E2）
  - 动作：`max_concurrent_calls` 收紧到 `<= 16`；`DynamicTool` 对 `Exec`/`Net` 默认强制 `Permissions::AskUser`
  - 验收：集成测试 `mcp_concurrency_ceiling_16`、`mcp_dynamic_tool_requires_permission`
  - 关联：O-4 / 安全审查 P1-1

- [ ] **T-8-2 CNB `auth.mjs` token 原子写**（P1）
  - 位置：`tools/cnb-mcp-enhance/auth.mjs:86-87`
  - 现状：`await mkdir(...)` + `await writeFile(TOKEN_FILE, ...)` 直写，非原子 ⇒ 并发刷新可产生 partial write / token 丢失（E2）
  - 动作：改为 `tmp` + `rename`（与 `crates/qaqh-config/src/secrets.rs:336-346` 的 `next_temp_path` + `write_doc` 同种机制）
  - 验收：lint 测试 `auth_refresh_token_atomic`；模拟并发刷新 → token 不丢
  - 关联：安全审查 P1-2

- [ ] **T-8-3 审计与账本键收尾**（P2）
  - 位置：`crates/qaqh-workspace/src/audit.rs:31-59`（无 rotation）、`crates/qaqh-workspace/src/file_state.rs:128-158` + `crates/qaqh-workspace/src/lib.rs:424-441`（键未 canonicalize）、`tools/cnb-mcp-enhance/server.mjs:87`（`repoPath` 无校验）、`:299/:438`（`state_reason` 硬编码）
  - 现状：`audit.csv` 只 append、无大小上限；`resolve_workspace_path` 对绝对路径原样返回不做归一（`lib.rs:430-432`）+ 符号链接不解析 ⇒ 账本键可能分歧；`repoPath = (repo) => \`/${repo || DEFAULT_REPO}\`` 无 `^[A-Za-z0-9_./-]+$` 校验；`cnb_issue_close` 硬编码 `state_reason=completed`（E2）
  - 动作：① `audit.csv` 加大小上限 + rotate；② 账本键改为 `canonicalize(resolve_workspace_path(raw))`（注意 Windows 大小写与符号链接语义）；③ `repoPath` 加字符白名单校验；④ `state_reason` 改为显式参数
  - 验收：单测 `audit_csv_growth_bounded`、`file_state_key_matches_resolved_key`；`rg -n 'state_reason' tools/cnb-mcp-enhance/server.mjs` 出现参数来源
  - 关联：O-5 / 安全审查 P1-3、P2 表

---

## 批次 9：清单归档与卫生（文档，可独立下发）← 不涉及代码

- 依赖：无
- 涉及：`docs/buglist/*`

- [ ] **T-9-1 归档 5 条状态过期条目**
  - 现状：`2026-09-14-timeline工具块内存放大-buglist.md` 的 `BUG-2026-09-14-01` ~ `-04` 仍写 `fixed（工作区，待提交）`，实际已随 `ea6063c`（2026-09-15）提交且是 HEAD 祖先；`2026-09-15-热重载吞首次变更与资源文案-buglist.md:17` 的 `BUG-2026-09-15-09` 同样，实际已随 `d9fa81c` 提交
  - 动作：状态改为 `fixed @ea6063c` / `fixed @d9fa81c`；同文件的「修复优先级 → 已完成（工作区，待提交）」小标题一并更新；表头「复核基线 `1c92413`」注明那只是登记提交
  - 验收：`rg -n '工作区，待提交' docs/buglist/` → 不再命中已提交项
  - 关联：D-20

- [ ] **T-9-2 修正 `BUG-2026-09-17-06` 的描述**
  - 现状：该条称 TUI `apply_status` 收到 `CANCELLED` 后不停跟踪；实测 TUI 仓 `~/Projects/qaqh-tui-app` @ `33253a5` 的 `src/app/mod.rs:844-856` 已调用 `untrack_subagent`（定义 `src/app/subagent.rs:352-356`，接线来自 `f85c33d`，2026-09-09 即已在历史里）
  - 动作：标注「HEAD 不成立；日志证据与代码事实不一致，需先确认运行的是哪个版本的二进制」，或直接关闭
  - 验收：该条状态不再是裸 `open`
  - 关联：D-21

- [ ] **T-9-3 修正 `BUG-2026-09-13-13` 的状态**
  - 现状：标 `✅ fixed @3775a9c`，但 chat 路径 `crates/qaqh-gate/src/chat_completions_api.rs:695-697` 仍缺 `null→{}` 兜底与 name 过滤 ⇒ 应改为 `PARTIAL`（或拆出新条目）
  - 动作：改状态 + 注明残留子情形与对应位置
  - 验收：状态列含 `PARTIAL` 或新条目已登记
  - 关联：D-18

- [ ] **T-9-4 清单卫生四项**
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
