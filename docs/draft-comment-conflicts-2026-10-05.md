# 注释冲突审计与修复 — 草稿

- **状态**：草稿（draft）。改动**未提交**，待 user 复核 diff 后决定是否落库。
- **日期**：2026-10-05
- **范围**：`crates/` 全部 442 个 `.rs` / 17,390 条注释
- **方法**：Rust 词法级注释提取（排除字符串内 `//`、raw string、嵌套块注释）→ 30 个平衡分片
  → 30 个子代理并行逐条对照源码裁定 → 对 18 条 high 逐一以代码/测试证据（必要时 `git blame` 追溯
  注释与代码引入时间）裁断后修注释。
- **约定**：每条结论附 `路径:行号`。审计中间物在
  `C:\Users\tsy3m\AppData\Local\Temp\qaqh-comment-audit\`（`INDEX.md` + `report/unit-01..30.md`），未写入仓库。

## 结论概览

- 30/30 分片均有发现，合计约 **203 条**冲突（high 18 / med 94 / low 91）。
- 三轮全部走完后：**189 条注释已改**（high 17 + med 87 + low 85），
  8 条跳过（3 条已被前轮顺带修掉、1 条报告误报、3 条纯措辞或全仓约定、1 条错侧是字符串常量），
  **5 条判为疑似代码有误**（§3；2026-10-05 复核后全部处置：§3.1 随 DSML 移除消解，
  §3.2/3.3/3.4/3.5 已修，详见各小节）。
- 全部改动**只落在注释行**，未触碰任何代码、测试断言、schema 或面向模型/用户的字符串常量。

## 1. 已修（17 条 high，15 个文件）

| # | 文件:行 | 原文 → 新文 | 裁断依据 |
|---|---|---|---|
| 1 | `crates/qaqh-config/src/config.rs:184` | `per-server 并发上限；1..=64` → `1..=16` | 校验器 `n > 16` 即报错；mcp 侧同为 16；同文件 276 行已述「上限从 64 收紧」 |
| 2 | `crates/qaqh-config/src/config.rs:981` | 「已有条目**不覆盖**——profile 为权威」→「**无条件覆盖** 同名条目——扁平值为最新意图」 | 代码 `profiles.insert(active, fallback)` 必覆盖；测试 `mixed_shape_flat_wins_then_converges` 锁定；同 commit 468cf59 |
| 3 | `crates/qaqh-config-api/src/lib.rs:28` | 「`serde(default)` 保证旧 daemon 缺字段时向前兼容」→「不做向前兼容——缺字段即解析失败」 | struct 无 `serde(default)`；测试 `dto_rejects_a_partial_payload` 断言缺字段失败；本文件 2026-09-15 兼容政策推翻该句 |
| 4 | `crates/qaqh-daemon/src/axum_server/axum_impl/control.rs:3-5` | 「网关是独立进程，仅由 `qaqh-daemon webui` 暴露」→「浏览器网关已随 Tauri 化移除；无 `webui` 子命令」 | `main.rs` 子命令表无 `webui`；`v2.rs:335`/README 均述网关已移除 |
| 5 | `crates/qaqh-daemon/src/axum_server/axum_impl/control.rs:15-17` | 「与 /health 同级免鉴权」→「**需 Bearer 鉴权**（不同于免鉴权的 /health）」 | handler 首行 `is_authorized` → 401；测试 `activity_requires_auth` |
| 6 | `crates/qaqh-daemon/src/axum_server.rs:2283` | 「WebUI 只能走独立 `webui` 网关」→「浏览器网关已随 Tauri 化移除」 | 同 #4 |
| 7 | `crates/qaqh-mcp/src/adapter.rs:184` | 「http → M3 占位错误」→「http → streamable HTTP 传输（PR-M3-1，含 `unix://`）」 | `McpTransportKind::Http` 分支已实现（`adapter.rs:213`） |
| 8 | `crates/qaqh-mcp/src/adapter.rs:6` | `http → M3 占位错误` → `http → streamable HTTP` | 同源句，随 #7 一并修 |
| 9 | `crates/qaqh-subagent/src/host.rs:561` | 「未安装返回 None（调用方回退 HTTP/SSE 路径）」→「即报错——legacy HTTP/SSE 降级已随 PR-4-2 删除」 | 同文件 `:15`、`lib.rs:9/912` 均述降级已删；所有调用点 `.ok_or_else` 直报错 |
| 10 | `crates/qaqh-runtime/src/agent/engine_input.rs:248` | 「`command_id` 是持久 injection-journal 键…标记 committed 防崩溃重放」→「透传 submit 作命令标识；注入日志已退役（PLAN B1）」 | 全仓无写 `injections.jsonl` 处；同函数 `:305`、`loop_core.rs:546` 述已退役 |
| 11 | `crates/qaqh-runtime/src/service.rs:642` | 「spawn 前写入 workspace.txt，子 worker 读到」→「workspace 作 cwd 落进子会话 meta…workspace.txt 已退役」 | 全仓无 txt 写侧；`load_session_workspace` 读 `meta.cwd`；`audit-legacy-protocol-2026-10-04.md:172` 已列该行为过时 |
| 12 | `crates/qaqh-runtime/src/ringing/timeline_hub.rs:320` | 「seal 的**即时裁剪**会把 journal 条目全删（设计如此）」→ 保留 #42 史实并标注「该即时裁剪已在 #314（2026-09-23）移除，因果链仅作历史背景」 | `timeline.rs:993-1006` 明文「不再在 seal 时裁剪」；`persistence_policy.rs:30` 记 #314 |
| 13 | `crates/qaqh-runtime/src/ringing/persistence_policy.rs:9-20` | 三频道 Reliable「✅ 三频道 journal / 投影恢复权威」等 3 行 → 合并为「❌ 不再落盘（v1 广播与事件 journal 持久化已删除）」；`:43` 删「与三频道 journal 对齐」 | `hub.rs:524`「v1 广播与事件 journal 持久化已删除」；`is_snapshot_persisted` 只收 `TimelineEvent` |
| 14 | `crates/qaqh-runtime/src/agent/loop_outcome.rs:574` | 「每 lap 轮询可让 CompactEnd 不拖到 turn 结束」→「lap 中 phase≠Idle 会提前返回，此调用是安全网，仍在 Idle 边界生效」 | `check_pending_compact` 守卫要求 `phase == Idle`；lap 期间为 GateRunning/ToolsRunning；G4 注释（`:37`）为准 |
| 15 | `crates/qaqh-workspace/src/apply_patch_engine/mod.rs:12` | 「any failure rejects the whole patch (all-or-nothing)」→「逐 hunk 落盘，失败保留已写文件（non-atomic — 见 `EngineError::Partial`）」 | 逐 hunk `apply_hunk` 落盘 + `Partial{applied}`；回归测试显式断言「非原子为真」；`apply_patch.rs:7` 同述 |
| 16 | `crates/qaqh-workspace/src/tool_api/legacy.rs:18` | 「适配器不写线程局部；`workspace_root` 暂不被消费」→「适配器把显式 ctx 装成线程局部兼容视图，`workspace_root` 会被消费」 | `execute_legacy` 调 `install_tool_call_context(ctx)`；测试 `execute_legacy_installs_explicit_context_for_handler` |
| 17 | `crates/qaqh-workspace/src/todo/mod.rs:4` | 「契约v3：todo_write（追加+空清空）」→「契约v4：todo_write（items 即完整清单，整体替换）」 | schema `replaces the previous list entirely`；`handle_write` 拒绝顶层 id/status；测试「全量覆写——替换而非追加」 |
| 18 | `crates/qaqh-client/src/endpoint.rs:416` | 「读端回退由 daemon `session_param_value` 承担」→「只认 `session_id`，legacy `seed` 已退场」 | `service_methods.rs:35-37` 无 seed 分支；测试 `session_param_only_accepts_session_id` 断言 `seed`→None |

## 2. 工作区说明

- 上述 15 个文件的改动**全部为注释行**，未改任何代码。
- `crates/qaqh-daemon/src/axum_server.rs` 与 `crates/qaqh-workspace/src/apply_patch_engine/mod.rs`
  在本轮之前就存在 user 的**未提交工作**（新增 resume 回归测试、cross-workspace admission 测试等）；
  本轮的注释改动与它们混在同一 `git diff` 里，提交时勿一并卷入。
- 本轮未创建任何提交。

## 3. 待拍板：疑似代码有误（5 条 → 全部处置完毕 2026-10-05）

> **2026-10-05 复核更新**：5 条全部对照当前代码核实属实，已全部处置——§3.1 随 DSML 移除
> 消解；§3.2 / §3.4 / §3.5 按 user 确认修掉；§3.3 按 user 拍板采纳"修订约束口径"方案修掉
> （各小节附修法与验证记录）。

### 3.1 `crates/qaqh-gate/src/chat_completions_api.rs:689`

- **【已消解 2026-10-05】** `parse_dsml_tool_calls` / `parse_xml_tool_calls` 已随 DSML
  支持整体移除而删除，契约注释不复存在，建议的一行修法随之作废。

- 现象：`parse_dsml_tool_calls(&text_buf, &[])` 传入**未剥离 markdown 围栏**的原文。
- 契约：`tool_parser.rs:31`（`parse_xml_tool_calls`）与 `:248`（`parse_dsml_tool_calls`）均写明
  「Caller MUST strip markdown code fences before passing content」；`strip_fenced_code` 即为此提供。
- 对照：`crates/qaqh-runtime/src/agent/util/format.rs:87` 正确地先 `strip_fenced_code` 再解析。
- 后果：``` 围栏内的 DSML 示例会被当作真实工具调用解析。
- 裁断：契约注释与本调用**同属 commit `d5ba44c`（2026-08-22）**，即上线第一天就违反自身契约，
  无法用「注释过时」解释 → 判为疑似代码 bug，不以改注释掩盖。
- 建议修法（一行）：先 `let stripped = crate::tool_parser::strip_fenced_code(&text_buf);`，再传 `&stripped`。
  改代码会动行为，等 user 确认。

### 3.2 `crates/qaqh-types/src/provider.rs:34` 与 `crates/qaqh-gate/src/chat_completions_api.rs:962`

- **【已修 2026-10-05】** 复核发现 sync 路径实际有**两处**漂移（草稿只记了 Qwen 键名）：
  Qwen 发 `thinking: true`（应为顶层 `enable_thinking`）之外，MiniMax 还漏发 `reasoning_split`。
  修法：抽出流式 / sync 共用的 `apply_thinking_params`（chat_completions_api.rs），两路径收敛到
  单一实现；锁定测试 `apply_thinking_params_matches_provider_contract`（gate 15/15 过）。

- 现象：契约注释规定 Qwen 走顶层布尔 `enable_thinking`，**流式路径**（`chat_completions_api.rs:133-135`）照此发送，
  但**同步路径**（`:962`）发的是 `"thinking": true`。
- 裁断：三条（契约注释 + 流式实现 + 同步实现）同属同一 initial commit，属「契约被代码违反」，
  且同一 crate 内两条路径自相矛盾。
- 建议：修**同步路径**的键名为 `enable_thinking`（与流式路径、`provider.rs` 契约一致），而非改注释。等 user 确认。

### 3.3 `crates/qaqh-workspace/src/dashboard.rs:7`

- **【已修 2026-10-05，user 拍板采纳"修订约束"方案】** R-4 口径修订为：workspace →
  domain 仅限**被动投影记录**（`Dashboard*` + `CodeDeltaRecord` 显式豁免），禁止 domain
  **事件/行为**类型。三处落点：`dashboard.rs:7`（约束声明 + 与工具无关的验证口径，替换了
  已不可复现的内嵌 rg 命令）、`tool_api/context.rs:43`（引用表述同步）、`code_delta.rs:1`
  （豁免点自述）。**未移动 CodeDeltaRecord**（跨 crate 手术、收益低）。
- **修途中的两个附带发现（均为早前改动的漏网，已一并处置）：**
  1. 集成测试 `apply_patch_engine_scenarios::escape_probes::dotdot_beyond_depth_rejected`
     仍锁 apply_patch 引擎**旧契约**（引擎层词法拒绝越界路径）——上一轮"admission owns
     the boundary"反转只做在单元测试侧，集成目标从未被跑到。已反转为
     `dotdot_beyond_depth_applies_outside_after_approval`（落点用 tempdir 自控目录取证）。
     ⚠ 语义备忘：`normalize_lexically` 对越深 `..` 的 clamp 锚点是**文件系统根**而非
     workspace 根，旧测试的 probe 实际会把文件写到 `AppData\evil.txt`（失败运行已实证，
     残留文件已清理）——边界只能由 admission 层在落盘前拦截。
  2. `tool_sdk_parity.rs:22` 冻结数 19 过时（`journal` 加入后未同步，即 unit-16「19→20」
     漂移的测试侧残留）→ 更新为 20 并注明来源。
- 校验：qaqh-workspace 全部 16 个测试目标通过（lib 460 + 集成 42，`--no-fail-fast` 全跑）。
- 现象：注释称「本 crate 对 `qaqh_domain` 的使用仅限 `Dashboard*` 类型（grep 排除 dashboard → 0）」，
  实际存在 `qaqh_domain::CodeDeltaRecord` 多处使用（`code_delta.rs:6/26/41/52/72`、`execution.rs:18`）。
- 裁断：该句是**有效架构约束 R-4**（`tool_api/context.rs:43` 依据它放弃导入 `ConversationMode`），
  且注释（commit 468cf59，2026-09-07）**晚于** `CodeDeltaRecord` 用法（commit d5ba44c，2026-08-22）
  → 疑为代码违反 R-4，而非注释漂移。
- 建议：确认 `CodeDeltaRecord` 是否应移出本 crate 的 domain 依赖面，或为 R-4 明确例外。等 user 确认。

### 3.4 `crates/qaqh-runtime/src/agent/tool_runtime.rs:1233`

- **【已修 2026-10-05】** 复核补充两点影响面事实：同簇的 `format.rs:42` 也有一个读
  `title`/`subject` 的 legacy `"todo"` 显示死分支（已一并删除）；而 dashboard 即时刷新在
  `engine_tool.rs:810`（流式 UI 直调，匹配本就正确）与 `turn_lap/backfill.rs:117`
  （逐 round 无条件刷新）一直在工作，故本死分支用户可见影响有限。修法：判定收敛为
  `dashboard.rs::is_todo_tool`（engine_tool / tool_runtime 共用），删除 format.rs 死分支；
  锁定测试 `is_todo_tool_matches_only_active_trio`（runtime 273 过，唯一失败为既有的
  prompt_and_tool_defs_char_budget，与本次无关）。
- 现象：注释「Instant refresh for todo tools」称 todo 工具会即时刷新 dashboard，代码判定为
  `matches!(tool_name, "todo")`；但 todo 注册名是 `todo_write` / `todo_update` / `todo_list`
  （`qaqh-workspace/src/todo/split.rs:65/86/106`，`tests/todo_contract.rs:32` 断言无 `todo`）。
- 裁断：`"todo"` 是 legacy 名，该分支为**死代码**，即时刷新从未生效 → 代码笔误。
- 建议：改为匹配 `todo_write` / `todo_update` / `todo_list`（或统一前缀判定）。等 user 确认。
  注：同簇的 `tool_side_fold.rs` 退役名残留（见 3.5）与此相关。

### 3.5 `crates/qaqh-workspace/src/tool_side_fold.rs:73-75`

- **【已修 2026-10-05】** 复核发现比"名单陈旧"更强的结论：该白名单 arm **行为上整体是死代码**
  ——列出的名字返回 `None`（透传），兜底 `_ => None` 也是透传，名单纯作文档。修法：整个 arm
  删除，留注释说明"默认全透传、工具名事实源在 registration.rs"，杜绝名单再漂移。
  workspace 460/460 过（tool_side_fold 既有 11 测试不动即过，佐证行为零变化）。
- 现象：透传白名单仍列出**已退役**的 `todo` / `todo_create` / `todo_insert` / `todo_set`，
  且**漏列**现役的 `todo_write` / `todo_update`（`registration.rs` 注册表仅 `todo_list/todo_update/todo_write`）。
- 裁断：`tests/todo_contract.rs:18-19,30` 明确断言这四个名字已退役不得再暴露 → 注释/测试侧正确，
  错在代码侧白名单（陈旧残留，行为无害）。
- 建议：清理该白名单为现役三件套。等 user 确认。

## 4. med 修订（本轮，94 条 → 改 87 / 跳过 4 / 转疑似代码 2）

按分片委派、以报告中的代码证据裁定「错的那一侧」，全程只改注释行。逐分片结果：

| 分片 | med | 改 | 跳过 | 转疑似 | 代表 |
|---|---|---|---|---|---|
| 1 mcp | 6 | 4 | 2 | 0 | `manager.rs:32` 热重载「归 Phase 2」→ 已落地；`connection.rs:146` tools 缓存「关闭即清空」→ 仅 crash 清 |
| 2 runtime | 3 | 3 | 0 | 0 | `engine_title.rs:9` 挂点改为 `turn_completed`；`engine_session.rs:57` `SessionRestored` 已退役并改英文注释 |
| 3 config | 1 | 1 | 0 | 0 | `watch.rs:47` `publish` 调用方补 `reload_from_disk` |
| 4 runtime | 5 | 4 | 1 | 0 | `types.rs:241` `PendingState` 实为单个 shutdown 标志；`types.rs:364/443`、`registry.rs:265` 陈旧/错位文档。F2 判定误报（两个反向偏差各有代码佐证，不改） |
| 5 types | 3 | 3 | 0 | 0 | `MINIMAL_TOOLS` 8→7；UA 版本「手工 bump」→ 测试强制跟随包版本；`minimal:dsh` 下线 |
| 6 session | 3 | 3 | 0 | 0 | `tool_ledger.rs:3` 接线「留待后续」→ 已实现；`manager.rs:1280` 2^32 空间 → UUIDv7 |
| 7 message | 5 | 5 | 0 | 0 | `context_flow.rs` trailing 策略收窄；goal_source 误挂文档归位；`lib.rs:4` 「每个 push_* 返回 bool」全称断言收窄 |
| 8 runtime | 3 | 2 | 1 | 0 | `service/params.rs:6` seed 回退 → 只认 session_id；`timeline.rs:1463` seal 清空回放尾 → #314 后不裁剪。F3 已由上一轮修掉 |
| 9 runtime | 2 | 2 | 0 | 0 | `lifecycle.rs:208` env 名 → per-agent 字段；`:457` `minimal:dsh` |
| 10 ringing/message | 3 | 3 | 0 | 0 | `capability.rs:1` open payload v1→v2；`wal.rs:555/875` global→thread-local、注入机制 |
| 11 daemon | 1 | 1 | 0 | 0 | `timeline_api.rs:9` 「没有深翻页接口」→ 已有 `archive_turn_page` |
| 12 runtime tests | 3 | 3 | 0 | 0 | TurnSealed「调用线程同步落盘」→ issue #28 后入队；消息数「不增删」→ 恰多一条 |
| 13 runtime | 4 | 4 | 0 | 0 | rebuild message 置空；`service.rs:182/230` Closed 广播已删；`service.rs:393` `minimal:dsh` |
| 14 workspace | 6 | 5 | 0 | 1 | 沙箱标志 global→thread-local（4 处）；`file_query.rs:588` expected_hash 消费方改 write；`pipe.rs:46` settle 条件。**→ §3.3** |
| 15 workspace | 1 | 1 | 0 | 0 | dry-run「穷尽真 apply 失败」→ 限定只读预检 |
| 16 workspace | 2 | 2 | 0 | 0 | `permission.rs:5` 1–4→1–3 档；`journal.rs` Step.tool 补 copy_range/web_fetch |
| 17 workspace | 5 | 5 | 0 | 0 | legacy 错误码「原样保留」→ 条件保留；`typed.rs:322` 无 `remapped` 字段；`descriptor.rs:193` 交互工具允许零超时；`tool_api/mod.rs:3` 「不接生产工具」→ 已接入 |
| 18 client | 2 | 2 | 0 | 0 | `client.rs:450` `fetch_timeline_page` 文档块内 `before_turn`/错粘段；`v2_stream.rs:9` activate/deactivate |
| 19 workspace | 4 | 4 | 0 | 0 | `shell.rs` 注册优先级/别名降级；`execution.rs:828` ToolEffect 单变体→双变体 |
| 20 runtime | 3 | 3 | 0 | 0 | `ringing/mod.rs` 删 router/outbox/journal 三幽灵模块；`hub.rs:29` 容量只服务 timeline；`hub.rs:288` 文档归位 |
| 21 gate | 2 | 1 | 0 | 1 | `chat_completions_api.rs:738` convert_messages 误粘 filter doc。**→ §3.2** |
| 22 workspace | 1 | 1 | 0 | 0 | `registration.rs:51` Todo v3 → v4 |
| 23 workspace | 4 | 3 | 1 | 0 | `process_registry.rs:29` NoOsPid 仅墓碑；`runtime.rs:486/576` 错位文档归位/删除。F2 已由上一轮修掉 |
| 24 domain | 2 | 2 | 0 | 0 | `timeline.rs:607` ts-rs 导出；`timeline.rs:645` message 置空 |
| 25 title/policy/skills/lsp | 5 | 5 | 0 | 0 | skills body 仅 activate 读；LSP 索引门「剩余量」→ 固定 min(60, startup)；LSP error 渲染方 bridge→tool.rs |
| 26 client | 1 | 1 | 0 | 0 | `v2.rs:113` `TeamAgentStatus` 取值枚举补齐 |
| 27 gate | 3 | 3 | 0 | 0 | `sse.rs:1` 三协议共用解码器；`transport.rs:27` MAX_RETRIES「5 次重试」→ 1 原始 + 4 重试 |
| 28 runtime | 5 | 5 | 0 | 0 | `CompactDelta` → `CompactProgress`（3 处）；`loop_outcome.rs:329`、`loop_injection.rs:18` 错位文档归位 |
| 29 subagent/spy | 5 | 5 | 0 | 0 | 收集器「HTTP/SSE 分支」→ 仅宿主直连；`config.rs:12` 逐实例覆盖 → 仅 4 参数；spy 漏斗调用方改为 `workspace_audit.rs` |
| 30 session | 1 | 1 | 0 | 0 | 中央索引 `index.json` → `index.jsonl`（两处） |

跳过 4 条：3 条为上一轮 high 已顺带修掉（`adapter.rs:6`、`config.rs:184`、`todo/mod.rs`），1 条报告误报（unit 4 F2）。

## 5. low 修订（本轮，91 条 → 改 85 / 跳过 4 / 转疑似代码 2）

同样按分片委派、代码为事实、只改注释行。要点（逐分片全表见下）：

- **命名/符号漂移**：`MCP_*` 错误码注释大写 → 实产小写（unit 1，~20 处）；`owns_seed` → `owns_session`（unit 20）；
  `is_seed_taken`/`allocate_seed` → `is_session_taken`/`allocate_session`、`release_seed_claim` → `release_session_claim`（unit 6）；
  `spawn_pipes` → `spawn_server`（unit 25）；`OFFLOAD/{seed}` → `ringing-offload/`、`list_seeds/load_seed` → `list_sessions/load_session`（unit 8）。
- **失效引用/路径**：`rendering` 相关符号、`docs/current/*`（见下）、`qaqh-gate/src/responses.rs` → `responses_api.rs`（unit 22）、
  `registry::externalize_large_content` → `agent::loop_dispatch_conversation::externalize_canonical_content`（unit 20）。
- **数值/枚举**：内置工具数 19→20、去掉不存在的 `task` 工具举例（unit 16）；`MinimalTools` 相关、进度流取值去 `"mixed"`（unit 24）；
  `read_chunk` limit「64 KiB」→ 512 B（unit 10）；MAX_RETRIES「5 次重试」→ 1 原始 + 4 重试（unit 27）。
- **陈旧行号锚点**：`engine_turn.rs:910`→`:1326`、`L1285`→`:1622`、`lifecycle.rs:268`→`:426` 等（unit 2/9/12）。
- **doc 错挂归位**：`store.rs:28`（store 级说明误挂 `Step`）；`loop_injection.rs:541` 会话切换 doc 移回对应用例（unit 28）；
  `read_image/mod.rs:119`、`runtime.rs:239`（unit 23）。
- **时钟/语义**：`timeline_hub.rs:408/640` journal 措辞（unit 13）；`tool_side_fold.rs` 退役名（→ §3.5）。

跳过 4 条：2 条纯措辞（unit 2 F7、unit 21 无）、1 条错侧为**字符串常量**（unit 11 F4 `lease_required` 的 v1/v2 提示串，
面向调用方文案，不在注释可改范围）、1 条为**全仓文档锚点约定**（unit 30 F3，见 §6）。

## 6. 剩余候选（low，未处理）

- **`docs/current/*.md` 悬空引用**：**不算缺陷**。`docs/current/` 在 commit `4ffc362 "clean"` 被整体删除，
  但该前缀锚点在全仓 41 处 `.rs` 中仍被引用（architecture/status/decisions/debug-backlog…），
  且已有明确豁免记录 `docs/archive/handoff-permission-three-tiers.md:107`。属整仓文档清理事项，非单文件注释问题。
- **错侧为字符串常量**：`auth.rs:28` 的 `"open a Ringing v1 client session first"`（5 处消费方均为 v2 端点，
  `v2.rs:1190`/`command.rs:73` 同 `code` 提示为 v2）——需改**代码**而非注释，且涉及用户可见文案，单列待议。
- 各分片报告 `report/unit-*.md` 中未列入本轮的少量边界项（如已被前轮顺带修掉者）。

## 7. 校验

- **编译**：`cargo check --workspace --all-targets` → **exit 0**（Finished in 42.72s；11 个 warning 均为既有的未使用变量类，
  与注释无关）。覆盖 lib / 测试 / bin，故注释改动若误伤任何目标都会在此暴露。
- **抽查**：对「注释搬家」类高风险改动逐处复核落点——
  `loop_injection.rs:18`（描述迁至 `drain_pending_injections:300`）、
  `types.rs:241`（`PendingState` 与单 `shutdown` 字段一致）、`types.rs:365`/`378`（`AdmittedTool` 与 `TurnState` 各归其位）、
  `store.rs:28`（由 `///` 改为 `//`，不再附着 `Step`）——落点准确。
- **口径限制**：工作区本就有 user 的大量未提交代码改动（spy store、todo v4、permission、
  grep_tool 等），因此无法用「全仓 diff 逐行分类」判定本轮改动是否纯注释；改用编译 + 抽查。
  各分片报告均声明改动仅触及注释行。
- 本轮未创建任何提交。

### 7.1 all-targets 编译结果

- `cargo check --workspace --all-targets` → exit 0，`Finished dev profile ... in 42.72s`，无 error（11 warning 为既有项）。
