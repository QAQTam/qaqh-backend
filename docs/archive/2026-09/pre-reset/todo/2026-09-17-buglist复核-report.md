# buglist 全量状态复核（2026-09-17）

## 0. 元信息

| 项 | 值 |
|---|---|
| 报告日期 | 2026-09-17 |
| 分析对象 | 仓库 `/home/qaqtamsy/Projects/qaqh-backend`；HEAD `661e5f4`；工作树 `crates/` **零改动**（`git status --porcelain -- crates/` 为空），另有 5 个未跟踪文档路径。旁证仓库 `~/Projects/qaqh-tui-app` @ `33253a5` |
| 触发方式 | 机主：「`docs/buglist` 里有很多未决 buglist，先验证哪些是早就落盘的修复但没更新或下线清单，哪些是还在生产上的 bug/缺陷/安全问题」 |
| 执行者 | 主代理 + 3 个 codex-cli 子代理（A/B/C 按清单文件分组并行），主代理负责 2026-09-16/17 全部 open 条目 |
| 结论 | 20 份清单共 **91 条**：**54 条已修复且状态正确**、**5 条状态过期可归档**、**2 条部分修复**、**29 条仍在生产**（含 4 条 P0 安全、3 条 P0/P1 子代理取消、6 条工具契约）、**1 条条目描述与 HEAD 不符** |

## 1. 结论摘要

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） |
|---|---|---|---|---|---|
| D-1 | **P0** | open | 安全 / 越权读 | `qaqh-runtime/src/service/fs_git.rs:10-91` | `fs.read`/`fs.list` 无路径白名单，持 token 者可读列**任意绝对路径** |
| D-2 | **P0** | open | 安全 / 边界绕过 | `qaqh-workspace/src/manager.rs:525-549`、`safety.rs:14-24` | `exec` 判 `in_workspace` 恒真，`Destructive` 出工区拦截形同虚设 |
| D-3 | **P0** | open | 安全 / 凭据泄露 | `qaqh-daemon/src/axum_server.rs:14`、`debug_control.rs:105-106` | `/health` 泄露 token 长度；debug 桥向页面注入明文 token |
| D-4 | **P0** | open | 资源泄漏 | `qaqh-session/src/manager.rs:1380-1385` vs `:223-241` | `delete()` 不释放 `session_locks`，条目随删除次数无界增长 |
| D-5 | **P0** | open | 功能 / 误判终态 | `qaqh-runtime/src/registry.rs:361-430` | 子代理不注册 liveness，启动同秒即被判孤儿封禁 |
| D-6 | **P0** | open | 功能 / 取消失效 | `qaqh-subagent/src/lib.rs:533-595` | `did_cancel=true` 仍把完整结果注入父会话 |
| D-7 | P1 | open | 功能 / 取消失效 | `loop_injection.rs:196-220`、`engine_input.rs:261-262` | 系统注入无条件 `clear_cancel()` 并开新回合 |
| D-8 | P1 | open | 功能 / 取消不传播 | 全仓（`children`/`descendants` 零命中） | 父会话取消不传导到子 seed，也无子 seed 登记 |
| D-9 | P2 | open | 状态单调性 | `qaqh-workspace/src/process_registry.rs:478-486` | `mark_exited` 把 `Killed` 覆盖回 `Exited` |
| D-10 | P2 | open | 死代码 / 活表滞留 | `qaqh-runtime/src/ringing/hub.rs:752` | `mark_worker_dead` 零生产调用点，子代理退出后滞留活表 |
| D-11 | **P0/P1** | open | 静默数据丢失 | `qaqh-workspace/src/apply_patch_engine/mod.rs:185-186`、`apply_patch.rs:157` | `Add File` 覆盖已有文件无守卫；失败文案谎称「无部分应用」 |
| D-12 | **P0** | open | 静默丢内容 | `qaqh-workspace/src/edit/resolve.rs:34-35`、`matching.rs:102` | 行内片段被 Tier3 采纳后**整行**被替换，前后缀无提示删除 |
| D-13 | P1 | open | 契约未同步 | `edit/handler.rs:344/:356` | 描述仍宣传已删除的三个 kind，按描述调用恒 `PARSE_ERROR` |
| D-14 | P1 | open | 计量失真 | `token_calibration.rs:163-164`、`engine_compact.rs:432` | 图片 base64 按普通文本计入 token，带图会话压缩空转 |
| D-15 | P1 | open | 契约缺口 | `qaqh-gate`（`context_window`/pre-flight 均零命中） | 超限请求无本地 pre-flight，400 仍整轮 Fatal |
| D-16 | P1 | open | 功能 / 静默吞变更 | `qaqh-runtime/src/service.rs:733-734`、`:775-776` | daemon 首次改 `config.toml` 的 `[lsp]`/`[mcp]` 变更被丢弃 |
| D-17 | P2 | open | 误导性文案 | `qaqh-mcp/src/resources.rs:181-191` | 「已连接但资源为空」被报成 "server not connected yet" |
| D-18 | P1 | **部分修复** | 协议不一致 | `chat_completions_api.rs:695-697` | chat 路径缺 `null→{}` 兜底与 name 过滤，`arguments:"null"` 可落盘 |
| D-19 | P1 | open | 资源泄漏 | `qaqh-lsp/src/manager.rs:202-215` | LSP 连接表只增不减（清单已承认、未立项） |
| D-20 | — | **状态过期** | 清单维护 | `docs/buglist/2026-09-14-…` ×4、`2026-09-15-热重载…:17` | 5 条已落盘修复仍标「工作区，待提交」 |
| D-21 | — | **描述不符** | 清单维护 | `docs/buglist/2026-09-17-…:22` | BUG-06 描述的缺陷在 TUI HEAD 已不存在 |

> 未列入本表的 54 条（2026-09-12/13 两批 + 09-15 client 系列 + read_image）经逐条核实**修复在位、无回退**，状态列正确，只需归档，见 §4 次要观察。

## 2. 分析方法与证据链

**工具**：CodeGraph（`.codegraph/` 存在，符号级定位）、`git log/show/merge-base -S`、逐行读 HEAD 源码、`rg`。**未执行** `cargo build/test/clippy`（子代理沙箱 read-only；主代理未跑以保持工作树零改动），因此所有性能类结论均为「结构性改动在位、收益未实测」的边界表述（E2），不冒充 E1。

**收敛路径**：

1. 先把 20 份清单里所有 ID 化条目抽出来（91 条），并按状态列分桶：声称 `fixed @commit` 的、声称 `open` 的、声称「工作区待提交」的。
2. 对**所有被引用的 commit** 做存在性与祖先性检查（`git cat-file -e` + `git merge-base --is-ancestor`）。结果：42 个 commit 全部存在且是 HEAD 祖先 ⇒ **不存在「声称修了但提交不存在」的情况**，问题只可能在「提交内容与声称不符」或「后续被回退」。
3. 对每条 `fixed` 条目：`git show <commit> --stat` 确认改动落点 → 读 HEAD 当前代码确认修复仍在 → `git log <commit>..HEAD -- <path>` 排除回退。
4. 对每条 `open` /「待提交」条目：直接读 HEAD 代码判断缺陷是否仍在；若已不在，用 `git log -S"<符号>"` 反查修复提交。
5. 分组并行：子代理 A（hidden-bug-scan 31 条 + exec 管道 4 条）、B（timeline/多会话 16 条）、C（2026-09-15/16 client-gate-tool 11 条）；主代理负责 2026-09-16/17 的安全审查、子代理取消、edit/apply_patch、热重载共 30 条。
6. 主代理对三份子代理报告的**每一条关键结论做独立抽查**（提交祖先性、对照分支代码、行号实读），抽查项全部复核通过。

**证据等级分布**：D-1~D-20 主结论均为 **E2**（逐行读通 + 提交归属）；`exec` 管道文件的 7 条未闭环观察为 **E3**（见 §5）；D-21 为 E2 且附旁证仓库 commit。

## 3. 发现详情

### 3.1 D-1 `fs.read` / `fs.list` 无路径白名单（E2）

- **现象**：`fs.list` 可枚举任意绝对目录，`fs.read` 可读取任意文件（上限 8 MiB）。
- **根因**：`crates/qaqh-runtime/src/service/fs_git.rs:10-64` 与 `:68-91` 只校验 `is_absolute()`；源码注释 `:8-9` 自述「临时跨端版本有意不做路径沙箱/权限校验」。入口 `crates/qaqh-runtime/src/service.rs:232-243` 直接把 IPC 参数透传，无归属校验。
- **影响面**：持有效 token 的任意客户端（含被 XSS 的 web 壳）可读取 daemon 进程可达的整盘文件；`fs.list` 还会枚举 `{data_dir}/sessions/{seed}/…` 命名，泄露命名空间契约。
- **复现**：`sed -n '7,14p;68,76p' crates/qaqh-runtime/src/service/fs_git.rs` → 无白名单分支。
- **修复建议**：最小 diff 是在 `fs_git.rs` 两个入口各加一道 `allowed_roots(workspace_root, data_dir)` 前缀校验（组件级比较，复用 `qaqh-workspace/src/permission.rs:375` 的 `path_within_dir`），拒绝时返回 `FORBIDDEN` 而非 `IO_ERROR`；并显式复用 `is_sensitive_session_path` 取代按 `meta.json` 子串过滤。
- **验收清单**：新增集成测试 `fs_read_rejects_meta_json`、`fs_list_rejects_sessions_dir`，断言返回错误码非 IO；`cargo test -p qaqh-runtime` 全绿。

### 3.2 D-2 Destructive 工具外工区拦截失效（E2）

- **现象**：`exec` 在工区外写文件不被 `SafetyPolicy` 拦截。
- **根因**（比清单原文更具体）：`crates/qaqh-workspace/src/manager.rs:375` 计算 `in_workspace = is_path_in_workspace(&ctx)`；该函数（`:525-549`）只读 `ctx.args["path"]`，`exec` 的参数名是 `command`，于是走 `:545-548` 的 `else` 分支**恒返回 `true`**（注释写着「assume workspace operation」）。`exec` 在 `crates/qaqh-workspace/src/exec/register.rs:29` 声明 `ToolRisk::Destructive`，而 `safety.rs:18-21` 只在 `(Destructive, false)` 时阻断 ⇒ 判定被 `true` 短路，永远放行。
- **影响面**：Level 3 下 `exec` 仍会弹审批（`permission.rs:533` 只对 `Write` 自动放行），但审批面板不显示目标路径（`extract_target_paths` 拿不到 shell 命令的写目标）；**Level 4（Unrestricted）则完全自动批准**（`permission.rs:489-491`），无任何提示。
- **复现**：`sed -n '525,549p' crates/qaqh-workspace/src/manager.rs`；`sed -n '14,24p' crates/qaqh-workspace/src/safety.rs`。
- **修复建议**：`is_path_in_workspace` 的 `else` 分支改为**按 risk 分级**——`Destructive` 工具缺 `path` 参数时返回 `false`（fail-closed），而不是 `true`；`Write`/`ReadOnly` 维持现状以免误伤 `task`/`skills`/`ask`。
- **验收清单**：新增单测 `destructive_tool_without_path_is_treated_as_outside_workspace`；e2e 覆盖 Level 4 下 `exec` 写工区外路径 → 期望被 `SafetyPolicy` 阻断。

### 3.3 D-3 daemon token / debug 桥暴露（E2）

- **现象**：`/health` 返回体含 `token_len`；`/debug/__qaqh_bridge__.js` 把明文 token 注入 `window.__QAQH_DEBUG__`。
- **根因**：`crates/qaqh-daemon/src/axum_server.rs:14` 的 `format!("ok epoch={} token_len={}", …)`；`crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs:105-106` 的 `"window.__QAQH_DEBUG__={{\"token\":\"{}\",\"nonce\":\"{}\"}};\n"`。
- **影响面**：token 长度泄露可用于缩小爆破面；桥脚本的明文 token 一旦被 XSS / DNS-rebinding 读走即等价于 daemon 完全接管。回环 + Host 双检（`debug_control.rs:283`、`:306`）已存在，但清单指出的三项加固（nonce 一次性兑换、`Sec-Fetch-Site`、常量时间比较）**未做**。
- **复现**：`sed -n '10,18p' crates/qaqh-daemon/src/axum_server.rs`；`sed -n '100,110p' crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs`。
- **修复建议**：`/health` 改为只报 `ok epoch={}`；桥脚本改为**只下发 nonce**，token 由客户端凭 nonce 走一次兑换接口换取，兑换后作废。
- **验收清单**：单测断言 `/health` 响应体不含 `token` 子串；`/debug` 响应体不含真实 token 字面量。

### 3.4 D-4 `session_locks` 泄漏（E2）

- **现象**：每删除一个会话，`session_locks` 表永久多留一条。
- **根因**：插入在 `crates/qaqh-session/src/manager.rs:1380-1385`（`session_lock()` 的 `entry(seed).or_insert_with(...)`）；`delete()`（`:223-241`）只做 `invalidate_watermark` / `remove_dir_all` / `remove_from_index` / `remove_session`，**不碰 `session_locks`**。`:1237` 的 `release_seed_claim` 清的是另一个 map（`claimed_seeds`）。
- **影响面**：长驻 daemon 下 `Arc<Mutex<()>>` 条目单调增长；同 seed 重建时会复用旧锁对象，与其他持有者无谓串行。
- **复现**：`sed -n '223,241p;1378,1387p' crates/qaqh-session/src/manager.rs`。
- **修复建议**：`delete()` 末尾加 `self.session_locks.lock()?.remove(seed);`，并同步 `WorkspaceStore::remove_session` 侧；注意与 `session_lock()` 的持锁顺序，避免引入反向获取。
- **验收清单**：新增单测 `session_locks_shrinks_after_delete`（删前/删后 `len()` 相等）；`cargo test -p qaqh-session` 全绿。

### 3.5 D-5 子代理不注册 liveness（E2）

- **现象**：子代理启动同秒被 bootstrap 的 orphan seal 判为孤儿并封禁，产生假 `cancelled=true` 终态。
- **根因**：`crates/qaqh-runtime/src/registry.rs:361-430` 的 `spawn_subagent_inprocess` 全文无 `hub.mark_worker_live(seed)`（`hub` 只在 `:386` 被 clone 后交给 event reader 线程）。对照另两处 spawn 路径**都有**：`:292`（`get_or_spawn`）、`:715`（`respawn_dead_agents`）。`orphan_seal` 的 liveness 门读的正是 `live_workers`。
- **影响面**：子代理任务被无声终止，父会话拿到"已完成/已取消"的错误结论；与 D-6 叠加即「取消后复活」观感。
- **复现**：`rg -n "mark_worker_live" crates/qaqh-runtime/src/registry.rs` → 只有 `:292`、`:715`，`spawn_subagent_inprocess` 内零命中。
- **修复建议**：`:425` 插入 `self.instances` 之后补 `self.hub.mark_worker_live(seed);`，并在 `finish`/`close` 路径补 `mark_worker_dead`（与 D-10 合并做）。
- **验收清单**：daemon 日志中 spawn 后出现 `worker alive for {seed}` 而非 `sealing orphan active turn`；新增单测 `spawn_subagent_registers_liveness`。

### 3.6 D-6 取消后仍注入结果（E2）

- **现象**：`did_cancel=true` 时子代理结果照样回灌父会话，触发父会话新回合。
- **根因**：`crates/qaqh-subagent/src/lib.rs:523` 只用 `did_cancel` 决定 `state_tag` 文案；`:543-586` 的注入块（`if !parent_seed.is_empty()`）**无 `did_cancel` 守卫**。且 `registry_ref.finish()` 在注入**之后**（`:597`），所以「已 finish」不等于「未注入」。
- **影响面**：用户在前端和后端都判定取消的子代理，仍会以 `state="cancelled"` 的完整答案开启父会话新回合 —— 即机主报告的「取消后复活」。
- **复现**：`sed -n '519,545p;595,600p' crates/qaqh-subagent/src/lib.rs`。
- **修复建议**：`:533` 条件改为 `if !parent_seed.is_empty() && !did_cancel`；若产品上希望"取消也留痕"，则改为注入一条**不含 `final_answer`** 的状态行。
- **验收清单**：daemon 日志在 cancel 路径下**不再出现** `inject accepted`；新增单测 `cancelled_collector_does_not_inject`。

### 3.7 D-7 系统注入清除取消标志（E2）

- **现象**：父会话被取消后，任何系统注入都会 `clear_cancel()` 并开新回合。
- **根因**：`crates/qaqh-runtime/src/agent/loop_injection.rs:196-220` 的 `LoopPhase::Idle` 分支无条件 `claim_injection` + `handle_system_input`，无「本会话是否已被用户取消」检查；`crates/qaqh-runtime/src/agent/engine_input.rs:261-262` 主动 `ctx.cancel.clear(); qaqh_workspace::clear_cancel();`。
- **影响面**：与 D-6 叠加构成复活链；即使 D-6 修好，其它系统注入源（compact 完成、崩溃恢复）仍可复活会话。
- **复现**：`sed -n '195,221p' crates/qaqh-runtime/src/agent/loop_injection.rs`；`sed -n '259,263p' crates/qaqh-runtime/src/agent/engine_input.rs`。
- **修复建议**：区分「用户取消」与「回合取消」两种语义（新增 `cancel_reason` 或 `user_cancelled` 位），系统注入仅在非用户取消态清除标志。
- **验收清单**：取消后父会话不再出现新的 `TurnStart`；单测覆盖「用户取消后系统注入被拒」。

### 3.8 D-8 取消不传播到子 seed（E2）

- **现象**：父会话取消时子代理继续运行。
- **根因**：全仓 `rg -n 'child_seeds|descendants|\bchildren\b' crates/` 在 cancel 路径零命中（仅 LSP 符号树与进程孤儿清理命中同名概念）；父会话**没有**「自身子 seed 集合」的登记能力，`crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs:95-124` 的取消处理只动父会话自身回合。
- **影响面**：取消一个正在 fan-out 的父会话后，子代理继续消耗模型配额并（经 D-6）回灌结果。
- **复现**：`rg -n 'children|descendants' crates/qaqh-runtime/src`。
- **修复建议**：在 registry 或 hub 侧登记 `parent_seed -> {child_seeds}`（spawn 时插入、`forget_seed` 时清理），cancel 处理里遍历并逐个 `cancel` + `mark_worker_dead`。
- **验收清单**：单测 `parent_cancel_propagates_to_children`；daemon 日志显示子 seed 收到取消。

### 3.9 D-9 / D-10 状态单调性与活表滞留（E2）

- **现象**：已 `Killed` 的进程条目可被改回 `Exited`；子代理正常退出后仍留在 `live_workers`。
- **根因**：`crates/qaqh-workspace/src/process_registry.rs:478-486` 的 `mark_exited` 无条件 `= ProcStatus::Exited(code)`，违反同文件 `:463-465` 注释自称的「status 单调，一旦离开 Running 不会回退」。`crates/qaqh-runtime/src/ringing/hub.rs:752` 的 `mark_worker_dead` **全仓零生产调用点**（仅 `:3417` 一个测试调 `mark_worker_live`），`live_workers` 只靠 `forget_seed`（`:791-793`）在会话关闭时清理。
- **影响面**：`is_running` 判定与孤儿清理依赖状态单调性，回退会让已杀进程被当成"刚退出"；活表滞留使 orphan seal 的 liveness 门长期误判。
- **复现**：`sed -n '463,486p' crates/qaqh-workspace/src/process_registry.rs`；`rg -n 'mark_worker_dead' crates/`。
- **修复建议**：`mark_exited` 加守卫——仅当当前状态为 `Running` 时才改写；`mark_worker_dead` 接入子代理退出路径（与 D-5 同一处改动）。
- **验收清单**：单测 `mark_exited_does_not_downgrade_killed`；`rg -n 'mark_worker_dead' crates/` 出现生产调用点。

### 3.10 D-11 `apply_patch` 三条（E2）

- **现象**：① 失败提示写「no partial application happened」，实际前序 hunk 已落盘；② `*** Add File:` 对已存在文件无守卫，直接整文件覆盖且不提"覆盖"；③ 同一上下文多处出现时静默改第一处。
- **根因**：① `crates/qaqh-workspace/src/apply_patch.rs:157` 的 hint 文案 vs `apply_patch_engine/mod.rs:181`（`for hunk in &hunks` 循环）+ `:273`（循环体内 `std::fs::write`）——逐 hunk 边算边写，非原子。② `mod.rs:185-186` 的 `Hunk::AddFile` 直接 `write_file_with_missing_parent_retry`，无 exists 检查；dry-run 侧 `:305-317` 也只拒绝目录；delta 的 `old: None`（`:191`）导致无回滚素材。③ `apply_patch_engine/seek_sequence.rs:39-43` exact 循环直接 `return Some(i)`，无歧义拒绝（对比 `edit` 会报 `Ambiguous`）。
- **影响面**：① 模型按提示重发完整 patch 必然二次 `NO_MATCH`；② **静默数据丢失**（5 行文件被 1 行覆盖，返回体无 overwrite 字段）；③ 改错位置且返回 `[OK]`。
- **复现**：`sed -n '150,160p' crates/qaqh-workspace/src/apply_patch.rs`；`sed -n '181,195p;270,280p;300,320p' crates/qaqh-workspace/src/apply_patch_engine/mod.rs`；`sed -n '36,46p' crates/qaqh-workspace/src/apply_patch_engine/seek_sequence.rs`。
- **修复建议**：① hint 改为陈述事实（「已生效：a.txt；未生效：b.txt — 只重发失败部分」）；② `AddFile` 加 exists 守卫，`dry_run` 返回 `WOULD_OVERWRITE`，并把旧内容读进 `FileDelta.old`；③ 工具描述补「同一上下文多处出现须补足上下文或用 `@@` 锚定」。
- **验收清单**：单测 `add_file_refuses_existing_path`、`failed_hunk_hint_reports_partial_application`；手动复现三条探针场景。

### 3.11 D-12 / D-13 `edit` 三条（E2）

- **现象**：① 行内片段 `old` 占所在行 ≥ ~85% 时被采纳，**整行**被 `new` 顶掉，未覆盖的前后缀无提示删除；② 达不到 0.85 时报误导性 `NO_MATCH`（`old` 逐字符就在候选行里）；③ 描述仍宣传三个已删除的 kind。
- **根因**：① `crates/qaqh-workspace/src/edit/matching.rs:102` 用 `similar::TextDiff::from_chars().ratio()` 做字符级评分，阈值 `edit/mod.rs:32` `T3_THRESHOLD = 0.85`；命中后 `edit/resolve.rs:34-35` 的替换区间是 `char_starts[start_line]..char_starts[start_line + win_lines]`，即**整个命中窗口**，不是 `old` 的行内位置。② `matching.rs:190-204` 只有「差阈值 / 差 margin / 完全不像」三种诊断，没有「片段 vs 整行」。③ `edit/handler.rs:344`（工具描述）与 `:356`（hunks schema）仍列 `insert_after`/`insert_before`/`replace_inline`，实现已在 `a92626d`（2026-09-08）删除；`edit/mod.rs:10`、`edit/transaction.rs:341` 同为残留。
- **影响面**：① **静默丢内容**且返回 `1/1 hunks applied … score 0.98`；③ 任何按描述发起的调用恒 `PARSE_ERROR: unknown hunk kind`。
- **复现**：`sed -n '96,104p' crates/qaqh-workspace/src/edit/matching.rs`；`sed -n '30,36p' crates/qaqh-workspace/src/edit/mod.rs`；`sed -n '26,40p' crates/qaqh-workspace/src/edit/resolve.rs`；`rg -n 'insert_after|replace_inline' crates/qaqh-workspace/src/edit/`。
- **修复建议**：① 让 Tier3 的替换区间锚定到 `old` 的实际行内 span，或在采纳前要求 `old` 覆盖整行；② 补第四种诊断「`old` 是行内片段，需给出完整行」；③ 描述与 schema 收敛到 3 个 kind，并补一条 `PARSE_ERROR` 的可读提示。
- **验收清单**：新增「片段 old」用例（清单自述 42 个 `#[test]` 里目前**一个都没有**）；`cargo test -p qaqh-workspace edit` 全绿。

### 3.12 D-14 图片 base64 计入 token 估算（E2）

- **现象**：带图会话每轮触发 auto-compact，压缩后仍超阈值。
- **根因**：`crates/qaqh-runtime/src/agent/state/token_calibration.rs:163` 把整份 `(messages, tools)` `serde_json::to_string` 后交 `:164` 的 `count_tokens`，而 `ToolResult.images[].data` 的内联 base64（`crates/qaqh-types/src/tool_result.rs:83-96`）就在这份字符串里，计数实现（`crates/qaqh-types/src/token.rs:19-28`）无图片感知。同一盲点第二处：`crates/qaqh-runtime/src/agent/engine_compact.rs:432-435`。
- **影响面**：1 MiB 级截图被折算成二三十万 token，而端点按像素只算几千 ⇒ `context_limit` 闸门对带图会话失效；每次压缩改 `message[1]` 导致 prompt cache 反复失效。方向是**高估**（过早压缩），故不表现为 400。
- **复现**：`sed -n '157,167p' crates/qaqh-runtime/src/agent/state/token_calibration.rs`；`rg -n 'image|base64' crates/qaqh-runtime/src/agent/state/token_calibration.rs` → 零命中。
- **修复建议**：在序列化前把图片字节替换为等价计费占位符（保守固定上限，改动最小、无契约变更）；`engine_compact.rs:432` 一并处理。
- **验收清单**：新增回归锁断言 `prepared_request_metrics` 不随图片字节线性增长（当前必然失败）；真机观察 `auto-compact preflight` 不再空转。

### 3.13 D-15 超限请求无本地 pre-flight（E2）

- **现象**：上下文超限时 400 整轮 Fatal，无兜底。
- **根因**：`rg "context_window" crates/qaqh-config crates/qaqh-types` 只命中文档，profile schema 仅有 `context_limit`（`crates/qaqh-types/src/config.rs:27`、`crates/qaqh-config/src/config.rs:71`）；`rg "CONTEXT_OVERFLOW|context_overflow" crates/` 零命中；`rg "context_limit" crates/qaqh-gate/src/` 零命中。唯一压缩触发路径 `crates/qaqh-runtime/src/agent/engine_turn.rs:880-925` 只做 `decision_tokens > limit × threshold`，**无「连续 400 ⇒ 强制压缩重试」分支**。
- **影响面**：清单 §「待办」四条（端点声明 `context_window`、超限 pre-flight、压缩硬兜底、chat/responses 双份图片去重）**一条都没做**；且清单记的「已把 `context_limit` 改成 240000」**已失效**——当前 `~/.config/qaqh/config.toml` 为 `context_limit = 1000000` + `auto_compact_threshold = 0.9`（阈值 900k），事故条件原样复现。
- **复现**：`rg -n 'context_window|context_overflow' crates/`；`sed -n '875,925p' crates/qaqh-runtime/src/agent/engine_turn.rs`；`rg -n 'context_limit|auto_compact_threshold' ~/.config/qaqh/config.toml`。
- **修复建议**：按清单优先级——先做「超限请求本地 pre-flight」（拿 `context_limit` 预判并在发请求前触发压缩），再做「端点声明上下文窗口」。
- **验收清单**：构造超限请求，期望**本地**触发压缩而非收到 400；`rg` 出现 `CONTEXT_OVERFLOW` 处理分支。

### 3.14 D-16 / D-17 热重载两条（E2）

- **现象**：daemon 启动后**第一次**改 `config.toml` 的 `[lsp]`/`[mcp]` 段被静默吞掉，第二次才生效；`list_resources` 把「已连接但资源为空」报成 "server not connected yet"。
- **根因**：① `crates/qaqh-runtime/src/service.rs:733-734`（mcp）与 `:775-776`（lsp）在真正处理循环（`:736`、`:778`）之前多了一次 `if rx.changed().await.is_ok() { rx.borrow_and_update(); }`；而 `qaqh_config::watch::subscribe()`（`crates/qaqh-config/src/watch.rs:34`）返回的 receiver 其 version 已是当前版本 ⇒ 那次 `changed()` 等到的是**用户启动后的第一次真实改动**，随即被 `borrow_and_update()` 丢弃。② `crates/qaqh-mcp/src/resources.rs:181` 的 `resources.filter(|r| !r.is_empty())` 把 `Some(vec![])`（已连接、无资源）与 `None`（未连接）合并成同一分支，`:187-190` 输出未连接文案；同文件的 `list_prompts` 区分正确。
- **影响面**：① 用户改配置看不出效果，且日志里 "reloaded" 字样造成"已生效"误判；每 daemon 生命周期各咬一次。② 误导模型/用户去调 `read_resource` 建立连接，而问题只是该 server 没有资源。
- **复现**：`sed -n '729,740p;772,782p' crates/qaqh-runtime/src/service.rs`；`sed -n '175,192p' crates/qaqh-mcp/src/resources.rs`。
- **修复建议**：① 删掉那两处前置 `changed()` 守卫（`subscribe()` 语义下该守卫的意图不成立）；② 把 `None` 与 `Some(empty)` 分成两个分支，后者输出「已连接，资源列表为空」。
- **验收清单**：① 重启 daemon 后第一次改 `[lsp]` 即出现 `[lsp] hot-reload applied`；② 对空资源 server 调 `list_resources`，文案不含 "not connected"。

### 3.15 D-18 `BUG-2026-09-13-13` 部分修复（E2）

- **现象**：清单标 `✅ fixed @3775a9c`，但 chat 协议仍可能落盘 `arguments:"null"`。
- **根因**：`3775a9c` **完全没有动 gate 文件**，而是换了层次——新增 `ContentBlock::is_hanging_tool_use()`（`crates/qaqh-types/src/message.rs:120-125`，判据**只看 id/name 是否为空**），在持久化前 `retain` 清洗（`crates/qaqh-message/src/store.rs:798`）并在回放投影过滤。主路径确已治愈。但 `crates/qaqh-gate/src/chat_completions_api.rs:695-697` 仍是 `serde_json::from_str(&args_json).unwrap_or(Value::Null)`，**无 `null→{}` 兜底、无 name 过滤**；对照 anthropic 侧 `crates/qaqh-gate/src/message_api.rs:739-745` 有 `if input.is_null() { json!({}) }`。
- **影响面**：流在「拿到 id+name、arguments 增量未到」时中断（`args_json == ""`）⇒ `input` 落成 `Null`，逃过 `is_hanging_tool_use` 清洗被持久化；出站序列化（`chat_completions_api.rs:823`）产生 `"arguments": "null"`，部分端点回 400。
- **复现**：`sed -n '689,700p' crates/qaqh-gate/src/chat_completions_api.rs`；`sed -n '738,746p' crates/qaqh-gate/src/message_api.rs`。
- **修复建议**：chat 的 Done 组装处对齐 anthropic——`if input.is_null() { json!({}) }`；清单把 `-13` 状态改为 `PARTIAL` 或拆出新条目。
- **验收清单**：新增单测 `chat_tool_use_null_input_becomes_empty_object`。

### 3.16 D-19 LSP 连接表只增不减（E2）

- **现象**：长驻 daemon 跨项目使用时 `conns` 表缓慢增长。
- **根因**：`crates/qaqh-lsp/src/manager.rs:202-215` 的 `get_or_connect` 按 `(server, root)` 插入连接后**从不摘除**；HEAD 上仅两处 remove：`:141`（配置变更）、`:309`（`shutdown_all`）。连接对象自身会被 `idle_shutdown_secs` 回收，但 map 条目保留。
- **影响面**：内存与路由键单调增长；清单里这条目前只出现在「误报」小节，容易被当成已处理。
- **复现**：`rg -n 'conns\.(remove|insert)' crates/qaqh-lsp/src/manager.rs`。
- **修复建议**：连接空闲回收时同步摘除 map 条目（路由键生命周期与连接生命周期绑定），或改为惰性摘除。
- **验收清单**：新增单测断言空闲回收后 `conns.len()` 回落。

### 3.17 D-20 清单状态过期（可归档，E2）

| 条目 | 清单写的 | 实际 |
|---|---|---|
| `BUG-2026-09-14-01` ~ `-04`（4 条） | `fixed（工作区，待提交）` | **已随 `ea6063c`（2026-09-15）提交**，HEAD 祖先；`crates/` 无未提交改动。独立验证：`engine_tool.rs:31` 已用有界 `tool_summary`（`timeline.rs:118-126`）、`timeline.rs:130/135` 双限、`enable_turn_offload` 已有 `timeline_hub.rs:261/293/319/533` 调用点、`journal_entry_payload_bytes`（`timeline.rs:850-864`）已计 `ToolUpdated` 的 summary+output+diff+progress |
| `BUG-2026-09-15-09`（README/justfile 漂移） | `fixed（工作区，待提交）` | **已随 `d9fa81c` 提交**（`git log -S'.deepx' -- README.md justfile` 指向该提交）；全仓 `.deepx`、`qaqh-msgloop` 已零命中，README 已写「16 个 workspace 成员」 |

### 3.18 D-21 条目描述与 HEAD 不符（E2）

`BUG-2026-09-17-06` 称 TUI `apply_status` 收到 `CANCELLED` 后「只改 `SubagentEntry.state`，不停止该 seed 的 timeline 跟踪」。实测 TUI 仓 `~/Projects/qaqh-tui-app` @ `33253a5`：`src/app/subagent.rs:207-215` 的 `apply_status` 返回 seed，调用方 `src/app/mod.rs:844-856` 随即调用 `self.untrack_subagent(&s)`（定义 `subagent.rs:352-356`，从 `subagent_seeds` 摘除并 `sync_tracked()`）。该接线来自 `f85c33d`（2026-09-09），是 HEAD 祖先。⇒ **该缺陷在当前 HEAD 不存在**。若真机仍观测到周期性 `GET .../timeline -> 401`，需先确认运行的是哪个版本的二进制（该条目引用的日志证据与代码事实不一致）。

## 4. 次要观察

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| O-1 | `qaqh-runtime/src/timeline.rs:437` | `tool.summary = summary.or_else(\|\| tool.summary.clone())` —— `summary` 为 `None` 时 clone 出的仍是 `None`，恒多余 | P3 | E2，清单已如实标 `open（残余）` |
| O-2 | `qaqh-runtime/src/timeline_store.rs:125-171` | sidecar offset 索引已落地（`load_offloaded_turn` 按 offset seek），但**长期大文件压力验证仍缺** | P2 | E2，清单状态与代码一致 |
| O-3 | `qaqh-runtime/src/timeline.rs:755-770` | `snapshot()` 仍 `turns.values().cloned().collect()` 全量深拷贝 | P2 | E2，清单已标 `open（残余）` |
| O-4 | `qaqh-config/src/config.rs:293-302`、`qaqh-mcp/src/connection.rs:717` | MCP `max_concurrent_calls` 校验范围是 `1..=64`，未收紧到 16；DynamicTool 权限层仍默认 allow-all | P1 | E2，见 D-1 同批安全清单 P1-1 |
| O-5 | `qaqh-workspace/src/file_state.rs:128-158`、`lib.rs:424-441` | 账本键统一走 `resolve_workspace_path`，相对路径形态已一致；残留缺口是**绝对路径原样返回不做归一** + 符号链接不解析 | P1 | E2，比清单原文窄 |
| O-6 | `qaqh-runtime/src/service/fs_git.rs:8-9` 等 | 多处「临时跨端版本」「有意不做沙箱」的自述注释，与安全审查结论直接冲突，建议随修复一并删除 | P3 | E2 |

**已核实修复在位、状态正确、可直接归档的 54 条**：

- `2026-09-13-hidden-bug-scan.md` `01`–`27`：30 个声明提交全部是 HEAD 祖先且改动在位，**无一条被回退**。
- 同文件 `28`–`31`（清单只写「✅ 已修」无提交号）：四条均定位到 `16f2c39`（「隐藏扫描补盲区」，HEAD 祖先）。注意 `-31` 与 `-08` 是同一缺陷的重复登记，实现符号是 `seal_unexecuted_as_cancelled` 而非清单写的 `finish_cancelled_batch`。
- `2026-09-12-exec…` `EXEC-01a`–`01d`：修复在位（`exec/pipe.rs:101-127`、`:136`、`exec/direct.rs:127/156/252`）。
- `2026-09-12-timeline死锁…` `01`–`03`、`2026-09-12-timeline快照…` `04`：修复在位（`timeline_hub.rs:593/361`、`debug_control.rs:283/306/335`、`bounded_read.rs:144-161`），回归测试多数已扩到比清单记载更多。
- `2026-09-12-多会话…` `08`–`15`：8 条修复在位（`hub.rs:399/403-535/1264`、`persistence_policy.rs:70-90`、`lease_store.rs:31/45-58/103-140`、`sse.rs:450-465/561-575`、`timeline.rs:156/870-884`、`tool_outbox.rs:70/146-160/400/417`、`store/mod.rs:104-271`）。
- `2026-09-15` 系列：`频道流epoch`01、`ts特性`06、`keepalive`07、`陈旧discovery`03/04、`深翻页`05、`daemon启动期`02；`2026-09-16-read_image`01；`2026-09-16-anthropic-400` 的次生问题①与待办⑤。共 9 条。

## 5. 不确定性与未验证假设

1. **`exec` 管道文件的 7 条未闭环项**（`H1a` spawn 期 stdio 接线异常、`H1b` 写端泄漏、`§C` daemon live 取证、`§E.2` 写端持有者枚举、`§E.3` 失败现场直捕、`§H` 遗留待复测，以及 `§A.2` 的关联观察）：**全部无法静态定论**，需 Windows 11 + 安装版 daemon + `%TEMP%\qaqh-exec-probe\` 工具集现场复现。已确认的只是 `30a011b` 把「Peek 失败即 break」这条确定性丢失路径改为排空到 `Ok(0)`，因此这些观测**不会**再表现为旧的「零字节 + `truncated:true`」。
2. **09-14 清单的 O-2**：声称 TUI 侧已随 `59541dd` 修复，但该提交与清单里的 Windows 绝对路径 `D:\project\qaqh-tui-app\…` 均不可达（`git cat-file -e 59541dd` 在本仓失败），未核。
3. **性能类条目的收益数字**（08 的 72k→110k ev/s、09 的 2 MiB→18.9 ms、12 的 62–247 ms、13 的 O(n) 阶跃、14 的吞吐、09-14 的 63 ms→0.01 ms）：本轮只确认**结构性改动在位**，**未复跑基准**，收益数字保持清单原值、未经本轮验证。
4. **D-2 的审批面板可见性**：我确认了 `is_path_in_workspace` 恒真与 `SafetyPolicy` 放行，但「用户在 Level 3 弹窗里能否看到目标路径」是从 `extract_target_paths` 对 `exec` 参数名的行为推断的（E2→接近 E3），未实机点击验证。
5. **`2026-09-16-安全并发与审查登记` 的 P2/P3 表**：只抽查了 `audit.csv` 无 rotation、`server.mjs` 的 `repoPath` 无校验、`cnb_issue_close` 硬编码三条（均确认仍在）；`ACTOR_WORKSPACE` 那条已被 `qaqh-workspace/src/runtime.rs:167-176` 的 `ActorToolScope` 部分缓解，**需重判**；其余两行清单行号已失效，未逐条重定位。
6. 子代理沙箱为 read-only，**未执行** `cargo build/test/clippy`；所有「回归锁在仓」的判断均为源码中存在该测试函数，而非跑绿。

## 6. 产物与复现物清单

| 路径 | 类型 | 是否落盘 | 说明 |
|---|---|---|---|
| `/tmp/qaqh-buglist-audit/A-report.md` | 子代理报告 | 是（临时） | hidden-bug-scan 31 条 + exec 4 条 + 正文 7 条 |
| `/tmp/qaqh-buglist-audit/B-report.md` | 子代理报告 | 是（临时） | timeline/多会话/09-14 共 16 条 + 4 条正文观察 |
| `/tmp/qaqh-buglist-audit/C-report.md` | 子代理报告 | 是（临时） | 2026-09-15/16 client-gate-tool 11 条 |
| `/tmp/qaqh-buglist-audit/{A,B,C}.log` | 子代理运行日志 | 是（临时） | 含每条结论的原始检索输出 |
| `/tmp/qaqh-buglist-audit/PROTOCOL.md` | 复核协议 | 是（临时） | 判定类别与证据要求 |
| `docs/todo/…-report.md` | 本报告 | 是 | — |
| `docs/todo/…-checklist.md` | 执行清单 | 是 | 本报告的待办拆解 |

**未修改仓库任何既有文件**；本轮新增仅 `docs/todo/` 下 3 个文件（README + 本报告 + 清单 + 命名标记文件）。

## 7. 后续工作与建议排期

| 序 | 工作 | 严重度 | 面 |
|---|---|---|---|
| 1 | 子代理取消链（D-5/D-6/D-7）——清单自述 D-5 是 1 行、D-6 是 1 个守卫，改动极小、止住最差的观感 | P0 | `qaqh-runtime` + `qaqh-subagent` |
| 2 | 安全 P0-1 / P0-4（路径白名单、锁表释放）——局部改动、影响面最大 | P0 | `qaqh-runtime` + `qaqh-session` |
| 3 | 安全 P0-2 / P0-3（fail-closed + token 不外泄） | P0 | `qaqh-workspace` + `qaqh-daemon` |
| 4 | `apply_patch` D-11 的 ②③（exists 守卫 + 描述补警示）与 `edit` D-13（描述收敛）——防静默数据丢失 + 纯文案 | P0/P1 | `qaqh-workspace` |
| 5 | `edit` D-12 的 ①②（替换区间锚定 + 第四种诊断） | P0 | `qaqh-workspace` |
| 6 | D-14 图片 token 估算 + D-15 超限 pre-flight | P1 | `qaqh-runtime` + `qaqh-gate` |
| 7 | D-16 热重载两条 + D-17 `list_resources` 文案 | P1/P2 | `qaqh-runtime` + `qaqh-mcp` |
| 8 | D-18 chat `null→{}` 兜底、D-19 LSP 连接表 | P1 | `qaqh-gate` + `qaqh-lsp` |
| 9 | 归档：D-20 的 5 条改状态、D-21 标注「HEAD 不成立」、`2026-09-12/13` 两批整体标记已闭环 | — | `docs/buglist` |
| 10 | 清单卫生：ID 冲突、哈希错配、示例内嵌伪造 ID 行、跨文件"遗留"段矛盾（见附录 B 第 6 组） | — | `docs/buglist` |
| 11 | **阻塞**：`exec` 管道 7 条需 Windows 现场；09-14 O-2 需 TUI 仓访问 | — | 需环境，不派 codex-cli |

## 附录 A：环境快照

| 项 | 值 |
|---|---|
| OS | Linux（子代理沙箱 read-only） |
| 主仓 | `/home/qaqtamsy/Projects/qaqh-backend` @ `661e5f4`（`main`） |
| 工作树 | `crates/` 零改动；5 个未跟踪文档路径（其中 2 份为本次复核对象的清单） |
| 旁证仓 | `/home/qaqtamsy/Projects/qaqh-tui-app` @ `33253a5` |
| codex-cli | `/home/qaqtamsy/Desktop/ws/codex`（`codex-cli 0.0.0`）——**后续一律用此路径** |
| 子代理模型 | 配置默认（`opencode-go` / `deepseek-v4.1-flash`，reasoning effort high） |

## 附录 B：复现命令

**1. 全局：被引用 commit 的存在性与祖先性**

```bash
cd /home/qaqtamsy/Projects/qaqh-backend
for c in 4e03a88 06d4caf 9ca245d 8afe3f5 1879db7 ff20292 806f013 a1e11e1 a920f90 \
         90d7051 fbb7f4d 4197db1 86264b7 3775a9c 6ade448 6b37056 8024577 e909220 \
         1e39069 766cd8d d262b8b ff5402d c578d44 ba9e0c0 6cbfbb9 537c098 0e63370 \
         1ccbb37 13cb21e 33e6261 fe4da88 acd638a 29f5a1b b7d5d6e 30a011b a72ce0c \
         4ac2f9c d9fa81c 9556aec 572f36a a9531ce 674742f 1c13662 16f2c39 ea6063c; do
  git cat-file -e "$c^{commit}" 2>/dev/null || { echo "$c MISSING"; continue; }
  git merge-base --is-ancestor "$c" HEAD && echo "$c OK" || echo "$c NOT-ANCESTOR"
done
```

**2. D-1 路径白名单缺失**

```bash
sed -n '7,14p;68,76p' crates/qaqh-runtime/src/service/fs_git.rs
sed -n '230,244p' crates/qaqh-runtime/src/service.rs
```

**3. D-2 `in_workspace` 恒真**

```bash
sed -n '525,549p' crates/qaqh-workspace/src/manager.rs
sed -n '14,24p' crates/qaqh-workspace/src/safety.rs
sed -n '488,492p' crates/qaqh-workspace/src/permission.rs
```

**4. D-3 token 暴露**

```bash
sed -n '10,18p' crates/qaqh-daemon/src/axum_server.rs
sed -n '100,110p' crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs
```

**5. D-4 锁表泄漏**

```bash
sed -n '223,241p' crates/qaqh-session/src/manager.rs
sed -n '1378,1387p' crates/qaqh-session/src/manager.rs
```

**6. D-5~D-10 子代理取消链**

```bash
rg -n 'mark_worker_live|mark_worker_dead' crates/
sed -n '361,430p' crates/qaqh-runtime/src/registry.rs
sed -n '519,545p;595,600p' crates/qaqh-subagent/src/lib.rs
sed -n '195,221p' crates/qaqh-runtime/src/agent/loop_injection.rs
sed -n '259,263p' crates/qaqh-runtime/src/agent/engine_input.rs
sed -n '463,486p' crates/qaqh-workspace/src/process_registry.rs
rg -n 'children|descendants|child_seeds' crates/qaqh-runtime/src
```

**7. D-11 `apply_patch`**

```bash
sed -n '150,160p' crates/qaqh-workspace/src/apply_patch.rs
sed -n '181,195p;270,280p;300,320p' crates/qaqh-workspace/src/apply_patch_engine/mod.rs
sed -n '36,46p' crates/qaqh-workspace/src/apply_patch_engine/seek_sequence.rs
```

**8. D-12 / D-13 `edit`**

```bash
sed -n '96,104p' crates/qaqh-workspace/src/edit/matching.rs
sed -n '30,36p' crates/qaqh-workspace/src/edit/mod.rs
sed -n '26,40p' crates/qaqh-workspace/src/edit/resolve.rs
rg -n 'insert_after|insert_before|replace_inline' crates/qaqh-workspace/src/edit/
```

**9. D-14 / D-15 计量与超限**

```bash
sed -n '157,167p' crates/qaqh-runtime/src/agent/state/token_calibration.rs
sed -n '428,438p' crates/qaqh-runtime/src/agent/engine_compact.rs
rg -n 'context_window|context_overflow|CONTEXT_OVERFLOW' crates/
rg -n 'context_limit|auto_compact_threshold' ~/.config/qaqh/config.toml
```

**10. D-16 / D-17 热重载与文案**

```bash
sed -n '729,740p;772,782p' crates/qaqh-runtime/src/service.rs
sed -n '30,36p' crates/qaqh-config/src/watch.rs
sed -n '175,192p' crates/qaqh-mcp/src/resources.rs
```

**11. D-18 chat 兜底缺口**

```bash
sed -n '689,700p' crates/qaqh-gate/src/chat_completions_api.rs
sed -n '738,746p' crates/qaqh-gate/src/message_api.rs
sed -n '116,126p' crates/qaqh-types/src/message.rs
```

**12. D-20 / D-21 状态过期与描述不符**

```bash
git log --oneline -S'.deepx' -- README.md justfile
git log -1 --format='%h %ad %s' --date=short ea6063c
sed -n '29,33p' crates/qaqh-runtime/src/agent/engine_tool.rs
cd /home/qaqtamsy/Projects/qaqh-tui-app && sed -n '844,856p' src/app/mod.rs && sed -n '351,357p' src/app/subagent.rs
```
