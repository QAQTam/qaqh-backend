# 隐藏 Bug 扫描报告 — 2026-09-13

> 静态审查（codegraph 索引 + 4 个产出子代理 + 人工复核），**未跑测试复现**。
> 编号沿用 `BUG-YYYY-MM-DD-NN` 惯例。状态：⬜ 待修 / ✅ 已修。
> 复核中被推翻的子代理结论见文末附录 B，勿按其修复。
> 上游 Codex 同问题修法对照见 `docs/report/2026-09-13-codex-parity-analysis.md`（相关条目内已标注 **Codex 参照**）。

## 汇总

| ID | 级别 | 位置 | 一句话 |
|---|---|---|---|
| BUG-2026-09-13-01 | P0 | qaqh-workspace/apply_patch_engine/mod.rs:129-155 | apply_patch `..`+不存在中间目录绕过 workspace 边界（安全绕过） |
| BUG-2026-09-13-02 | P0 | qaqh-gate/transport.rs:123 | 重试退避 `2u64.pow` 指数溢出（debug panic / release 重试风暴） |
| BUG-2026-09-13-03 | P0 | qaqh-daemon/axum_impl/content.rs:104,115,40 | 上传 media_type 无校验 → GET 响应头注入，axum panic（存储型 DoS） |
| BUG-2026-09-13-04 | P0 | qaqh-runtime/engine_input.rs:183-192 + qaqh-message/store.rs:277-289 | 用户图片不落盘，重启即丢（数据损坏） |
| BUG-2026-09-13-05 | P0 | qaqh-session/manager.rs:770-791 | save_full 重建 meta 丢 cwd/frozen_annotation/usage 等 7 字段 |
| BUG-2026-09-13-06 | P1 | qaqh-message/wal.rs:233 | read_ops 吞 IO 错误 + checkpoint 销毁证据（违反 fail-closed 契约） |
| BUG-2026-09-13-07 | P1 | qaqh-runtime/agent/loop_core.rs:547-565 | drain_pending 裸 dispatch 绕过 safe_dispatch（panic 逃逸 + liveness busy 失效） |
| BUG-2026-09-13-08 | P1 | qaqh-runtime/agent/turn_lap/admit.rs:156-161,216-218 | 恢复批取消路径丢弃已执行工具结果 → 工具重复执行 |
| BUG-2026-09-13-09 | P1 | qaqh-client/session.rs:191-201 | lease 续期请求无超时 → 自愈循环永久卡死 |
| BUG-2026-09-13-10 | P1 | qaqh-daemon/axum_impl/sse.rs:257-262 | timeline 实时流缺逐事件 owns_seed 复查（吊销后数据暴露窗口） |
| BUG-2026-09-13-11 | P1 | qaqh-config/secrets.rs:236-239 | secrets 固定 tmp 文件名，跨进程并发写丢 DPAPI 密钥 |
| BUG-2026-09-13-12 | P1 | qaqh-gate/transport.rs:262-272 | stateful 过滤死分支 → 空 messages 数组 400，回合 Fatal |
| BUG-2026-09-13-13 | P1 | qaqh-gate/chat_completions_api.rs:683-687 + message_api.rs:697-701 | 中断流悬挂 ToolUse（input:Null / name:""）进历史回放 |
| BUG-2026-09-13-14 | P2 | qaqh-workspace/permission.rs:461-470 | trust folder 精确匹配，信任目录新建子目录仍弹审批 |
| BUG-2026-09-13-15 | P2 | qaqh-workspace/permission.rs:108-115 | from_u8 fail-open：非法档位静默升级 Unrestricted |
| BUG-2026-09-13-16 | P2 | qaqh-workspace/copy_range.rs:321-323 | 账本键路径形态不一致，copy_range 写绕过防漂移 |
| BUG-2026-09-13-17 | P2 | qaqh-gate/sse.rs:61-93 + qaqh-client/sse_decoder.rs:67-70 | SSE BOM 吞首事件（两处独立变体） |
| BUG-2026-09-13-18 | P2 | qaqh-daemon/axum_impl/timeline_api.rs:13-25 | limit=0 空页 + has_more=true，客户端翻页死循环 |
| BUG-2026-09-13-19 | P2 | qaqh-gate/tool_parser.rs:164 | 单个畸形 `<parameter>` 废弃整个 invoke 调用（工具无声消失） |
| BUG-2026-09-13-20 | P2 | qaqh-gate/responses_api.rs:808-824 | sync 空内容返回 Ok("")，与另两协议 Fatal 语义相悖 |
| BUG-2026-09-13-21 | P2 | qaqh-gate/transport.rs:60 | sleep_with_cancel Duration 减法下溢（纳秒窗口 panic） |
| BUG-2026-09-13-22 | P2 | qaqh-gate/transport.rs:209-227 | retry-after 头无上限信任，回合可被挂起任意久 |
| BUG-2026-09-13-23 | P2 | qaqh-workspace/process_registry.rs:120-140 | 终态条目按 started 驱逐，丢 os_pid → 孤儿进程无法清理 |
| BUG-2026-09-13-24 | P2 | qaqh-session/manager.rs:897-910 | generate_seed 32 位无碰撞检查，碰撞即写穿旧会话目录 |
| BUG-2026-09-13-25 | P2 | qaqh-message/store.rs:699-702 | push_image_to_last_user 空 turns 静默丢图（无错误通道） |
| BUG-2026-09-13-26 | P2 | qaqh-session/grouping.rs:195-203 | workspace create() TOCTOU，并发建同路径重复注册 |
| BUG-2026-09-13-27 | P2 | qaqh-gate/message_api.rs:125-127 | user 消息兜底空 text 块被严格端点 400 |

## 详情

### BUG-2026-09-13-01 ⬜ P0 apply_patch 工作区逃逸
- **位置**：`crates/qaqh-workspace/src/apply_patch_engine/mod.rs:129-155`
- **机制**：patch 写 `*** Add File: a/b/../../evil.txt` 且 `a/b` 不存在（Add File 常态）→ `joined.exists()`=false、parent 也不存在 → 走 `_ => joined.clone()`，`abs` 含未消解 `..`；L150 `starts_with` 纯词法前缀比较照常通过 → 返回 Ok，后续落盘时 OS 解析 `..`，写到 workspace 外。
- **修法**：resolve 入口先 `normalize_lexically`（permission.rs:290 已有现成实现），或拒绝含 `Component::ParentDir` 的路径；`canonicalize().unwrap_or(joined)` 降级必须走 Err 而非原样放行。
- **验证**：集成测试——patch `Add File: a/b/../../evil.txt`，断言 Err 且 `evil.txt` 不存在于 workspace 外。
- **Codex 参照**：`PathUri::join` 词法消解 `..` 且 clamp 在锚点内（docs/report/2026-09-13-codex-parity-analysis.md BUG-01 节）；修法可升级为路径收敛进归一化新类型（同时消除 -16）。

### BUG-2026-09-13-02 ⬜ P0 重试退避指数溢出
- **位置**：`crates/qaqh-gate/src/transport.rs:123`（`let mult = 2u64.pow(attempt.saturating_sub(1));`）
- **触发**：`RetrySpec.max_retries` 来自 TOML 无上限校验（L103-105）。max_retries ≥ 66 时 `2u64.pow` 溢出：debug/测试构建 panic（`u64::pow` 内部 expect，非 overflow-checks 门控）；release 回绕（2^64 ≡ 0）→ 退避塌缩 0ms → 重试风暴。
- **修法**：`2u64.checked_pow(attempt.saturating_sub(1)).unwrap_or(u64::MAX)`；下游 `saturating_mul` + `min(max_delay)` 已能正确封顶。顺带给 max_retries 加配置上限（如 ≤ 32）。
- **验证**：`delay_for(66)` 负例测试。
- **Codex 参照**：`core/src/util.rs:86` 用 f64 `powi` + 饱和转换，等效防法（docs/report/2026-09-13-codex-parity-analysis.md BUG-02 节）。

### BUG-2026-09-13-03 ⬜ P0 content media_type 头注入 → handler panic
- **位置**：`crates/qaqh-daemon/src/axum_server/axum_impl/content.rs:104`（原样收下）、`:115`（仅缺省兜底无校验）、`:40`（直接拼 CONTENT_TYPE 响应头）
- **触发**：持有效 lease 的认证方上传 `media_type` 部件含 `\r\n`（如 `text/plain\r\nX-Evil: 1`）→ 入库；此后每次 GET 该 content，axum 元组响应（Error=Infallible）对非法头值 `TryInto<HeaderValue>` 失败 → panic。存储型 DoS，同 seed 反复复现。
- **修法**：上传时校验 media_type 为合法媒体类型 token（可见 ASCII、无 CRLF），非法 400 或回退 `application/octet-stream`；出站用 `HeaderValue::from_bytes` 失败兜底双保险。

### BUG-2026-09-13-04 ⬜ P0 用户图片不落盘
- **位置**：`crates/qaqh-runtime/src/agent/engine_input.rs:183-192`（图片在 ingest 后才 push）；`crates/qaqh-message/src/store.rs:277-289`（save_msg 克隆快照进 pending_save）；`:681-703`（push_image_to_last_user 只改内存 turn）
- **机制**：ingest 时持久化副本不含图片；push 只改内存。归档 JSONL 的用户消息永远无图片块；唯一补救 `snapshot_full` 全量重写仅在 undo/compact 触发。带图会话崩溃/重启后图片永久丢失（内存 ImageRef 指向的磁盘字节还在，但索引丢了）。
- **修法**：push 图片后重新入队该消息持久化（或把图片外置挪到 ingest 之前、Message 构造时带上）。
- **验证**：带 UI 消息 + 图片 → flush → 从磁盘重放 → 断言 ImageRef 在场。
- **Codex 参照**：`AttachmentStore` 契约——durable ref 是字节落盘的后置产物（docs/report/2026-09-13-codex-parity-analysis.md BUG-04 节）；中期可把 ImageRef 构造改为"字节已在场"的后置契约。

### BUG-2026-09-13-05 ⬜ P0 save_full 丢 meta 字段
- **位置**：`crates/qaqh-session/src/manager.rs:770-791`（`..Default::default()` 前只保留了 mode/skills/tool_mode/custom_tools/title）
- **丢失字段**：`cwd`、`frozen_annotation`、`usage_totals`、`last_usage`、`usage_requests`、`cache_reported_requests`、`context_stats`。
- **连锁**：undo/compact 一次 → 重启 resume 后 `load_session_workspace`（lifecycle.rs:11-18 读 meta.cwd）退回 `"."`；`restore_frozen_annotation`（agent.rs:527-528）拿不到注解 → 重新生成（日期变化）击穿 provider 前缀缓存——正是 session.rs:101-108 注释强调的 P0 cache fix 被自己冲掉。
- **修法**：与 tool_mode/title 同款，从 `existing` 保留全部持久化字段（或改为 `let mut meta = existing;` 再覆写需要更新的字段）。
- **验证**：undo → 断言 meta.json 中 cwd/frozen_annotation 保留。
- **关联**：持久化时序类缺陷，与 -04 同类（Codex durable ref 契约参照见 docs/report/2026-09-13-codex-parity-analysis.md）。

### BUG-2026-09-13-06 ⬜ P1 WAL read_ops 吞 IO 错误
- **位置**：`crates/qaqh-message/src/wal.rs:233`（`reader.lines().map_while(Result::ok)`）
- **机制**：中途 IO 错误（磁盘 EIO / Windows 共享冲突 / 杀软）被当 EOF，静默返回截断 op 集——无 quarantine、无日志，违反本文件 L214-216 自述 fail-closed 契约；调用方 `replay_message_wal` 随后 `checkpoint_file` 截断 WAL，未读到的有效 op 永久丢失。对比：JSON 损坏路径（L240-251）有 log + `fs::copy` 隔离。
- **修法**：显式 match `Result`，错误路径走与 JSON 损坏相同的 quarantine + break；更彻底是 `read_ops` 返回 `io::Result<Vec<_>>` 交调用方 fail-closed 决策。

### BUG-2026-09-13-07 ⬜ P1 drain_pending 裸 dispatch
- **位置**：`crates/qaqh-runtime/src/agent/loop_core.rs:547-565`（`drain_pending` 直派与 `dispatch_deferred_ringing` L570-581 循环体均无 safe_dispatch 包裹）
- **后果**：① 引擎 panic 从 drain_pending 直接穿透 run()（L404-476 无 catch_unwind），进程死亡，已取出命令 + 未 flush PersistOps 全丢；② liveness busy 标记不置位（liveness.rs:7-9 契约），`unload_idle_sessions` 可把正在工作的 worker 误杀；③ L554 首分支缺 `drain_persist_ops`。
- **触发**：每次主循环迭代开头（L411）先于 recv_timeout 运行——空闲期队首第一条命令（含 UserInput）都走无保护路径。
- **修法**：直派与 deferred 循环体抽成带 safe_dispatch 包裹（内含 drain_persist_ops）的 helper，两处调用。

### BUG-2026-09-13-08 ⬜ P1 恢复批取消丢弃工具结果
- **位置**：`crates/qaqh-runtime/src/agent/turn_lap/admit.rs:156-161`（并行批 `let _ = handle.join(); continue;`）、`:216-218`（串行 `return false`）
- **触发**：权限挂起（YieldToUser）→ 用户批准 → deferred 批执行中取消（`handle_permission_resolved` → `execute_admitted_batch`）。
- **后果**：工具副作用已发生（outbox 已记录）但结果不回填 → store 留 open tool_use；不 `remove_last_step_if_incomplete`、不 flush、无 Cancelled seal → 下轮模型重发 tool_use，工具重复执行；串行路径还丢弃已收集的 skill_effects。
- **对照**：同文件 `admit_and_dispatch` 的取消处理完整（L527-529 break + L738-748 统一收尾）。
- **修法**：取消分支照常收割回填已完成的 call_id；或至少对齐 admit_and_dispatch 的 remove_last_step_if_incomplete + flush_meta 收尾。L158、L216、L312-314 统一。

### BUG-2026-09-13-09 ⬜ P1 lease 续期无超时
- **位置**：`crates/qaqh-client/src/session.rs:191-201`（renew_once 的 `.send()` 无 `.timeout(..)`）
- **机制**：open 专门加了 `OPEN_TIMEOUT_SECS`（L40、L66-70 注释描述 daemon 挂起场景），renew 同场景无超时——一次 renew 挂起使 `run_renewal` 的 `tokio::select!` 永久停在该分支，ticker 不再触发 → lease 过期 → keepalive 闸门关流，失败计数/重新 open 逻辑（L154-179）永远走不到，自愈死循环。
- **修法**：renew_once 补 `.timeout(Duration::from_secs(OPEN_TIMEOUT_SECS))`（或专用常量，略小于 lease TTL）。

### BUG-2026-09-13-10 ⬜ P1 timeline 实时流缺逐事件所有权复查
- **位置**：`crates/qaqh-daemon/src/axum_server/axum_impl/sse.rs:257-262`（live 循环仅 `live.seed != seed_clone` 字符串比对）vs channel 流 `:147-153`（逐事件 owns_seed）
- **机制**：入口（L217-230）只查一次 owns_seed；长连接内 lease 若发生 seed 级 detach/吊销（错误文案 "attach the session seed" 暗示该生命周期存在），`is_active_session` 仍 true，timeline 新条目持续推给已失去该 seed 的会话。replay 段同理（窗口极小）。
- **修法**：live 循环内镜像 L147-153：`!leases...owns_seed(&session_id_clone, &live.seed)` 时 continue（可选连续 N 次后 break）。

### BUG-2026-09-13-11 ⬜ P1 secrets 固定 tmp 名并发丢密钥
- **位置**：`crates/qaqh-config/src/secrets.rs:236-239`（`self.path.with_extension("toml.tmp")`）
- **触发**：daemon 进程（webUI 保存配置）与 CLI 进程（`qaqh-daemon mcp import --exec`）并发写——`config_io_lock`（config.rs:629）是进程内 Mutex，跨进程无效。
- **后果**：rename 竞态 → 后写者用不含对方键的文档整体覆盖 → DPAPI 密文不可重生成，静默丢密钥。
- **修法**：tmp 名加 pid+nonce（参照 qaqh-workspace atomic_write file_shared.rs:50-54）；read-modify-write 加跨进程文件锁。
- **Codex 参照**：`secrets/src/local.rs:295` 的 `.tmp-{pid}-{nonce}` 与本修法逐字一致（docs/report/2026-09-13-codex-parity-analysis.md BUG-11 节），按原案执行。

### BUG-2026-09-13-12 ⬜ P1 stateful 过滤死分支 → 空 messages 400
- **位置**：`crates/qaqh-gate/src/transport.rs:262-272`
- **机制**：stateful provider 且最后一条消息是 assistant 时 `start == len`、`out` 必空，回退分支守卫 `last.role != "assistant"` 恒假（死代码）→ 三协议发 `"messages": []`（chat_completions_api.rs:88、message_api.rs:751）→ 400 不可重试 → 回合 Fatal。
- **修法**：`last.role == "assistant"` 时合成最小 user 续写消息（或显式报错）。

### BUG-2026-09-13-13 ⬜ P1 悬挂 ToolUse 进历史
- **位置**：`crates/qaqh-gate/src/chat_completions_api.rs:683-687`（`unwrap_or(Value::Null)` 直接入 blocks，无 name 过滤）、`message_api.rs:441-445`、`:697-701`（or_insert 造无名工具，L697 只兜 id 不兜 name）
- **触发**：流中断（读错误/空闲超时）时 tool_acc/tool_states 非空走抢救路径；或 provider 漏发 content_block_start。
- **后果**：`input: Null`（chat 无 anthropic 的 `null→{}` 兜底 L692-695）、`name: ""` → 下游执行必失败；进历史回放序列化 `arguments:"null"`，部分端点 400。responses 有防护（responses_api.rs:1106 `!call_id.is_empty() && !name.is_empty()`），属漏改。
- **修法**：Done 组装统一过滤 `name.is_empty()`；`stream_interrupted && stop_reason.is_none()` 时丢弃解析失败的工具调用；chat 补 Null→{}。

### BUG-2026-09-13-14 ⬜ P2 trust folder 子树失效
- **位置**：`crates/qaqh-workspace/src/permission.rs:461-470`
- **机制**：`resolve_target_path(trusted) == dir` 精确相等；信任 `D:\shared` 后写 `D:\shared\sub\new.rs`（sub 新建）→ parent()=`D:\shared\sub` ≠ trusted → Level 3 每次弹审批，与 "one-time trust" 语义相反；且两侧比较未做大小写归一化。
- **修法**：`dir.starts_with(resolve_target_path(trusted.clone()))` + Windows 大小写归一。

### BUG-2026-09-13-15 ⬜ P2 from_u8 fail-open
- **位置**：`crates/qaqh-workspace/src/permission.rs:108-115`（`_ => Self::Unrestricted`）
- **触发**：config.toml `permission_level = 0` / 5..=255（config.rs:859-861 不校验范围）→ 所有工具调用按 Level 4 免审批。
- **修法**：解析失败保守降级 MaxLockdown 或 config load 时 fail-fast 校验 1..=4。

### BUG-2026-09-13-16 ⬜ P2 copy_range 账本键形态不一致
- **位置**：`crates/qaqh-workspace/src/copy_range.rs:321-323`（`record_write(&target_path, ...)` 用原始参数路径）
- **机制**：read/edit 的账本键是 `resolve_workspace_path` 后的绝对路径（file_query.rs:53、edit/handler.rs:37）；copy_range 用相对路径原样记账 → 同文件两套键，STALE_FILE 校验对 copy_range 的写视而不见。
- **修法**：`record_write` 传解析后的 `tgt`。

### BUG-2026-09-13-17 ⬜ P2 SSE BOM 吞首事件（两处）
- **位置**：`crates/qaqh-gate/src/sse.rs:61-93`（BOM 行不匹配任何分支 → L93 静默忽略）；`crates/qaqh-client/src/sse_decoder.rs:67-70`（from_utf8 不剥 BOM，`\u{feff}id:` 前缀失配丢游标）
- **触发**：代理/网关注入 UTF-8 BOM 与首字段同行。daemon 自身发送端无 BOM，仅中间层注入场景。
- **修法**：解码器首次 push 前剥一次 BOM（或每行 strip `\u{feff}` 后再匹配）。

### BUG-2026-09-13-18 ⬜ P2 timeline limit=0 翻页死循环
- **位置**：`crates/qaqh-daemon/src/axum_server/axum_impl/timeline_api.rs:13-25`、handler 直通 `:113`
- **机制**：limit=0 时 end==start → page 恒空但 start>0 → has_more=true → has_more 驱动的客户端分页永不停歇。（若 `TimelineQuery.limit` 为 NonZeroUsize 则被 Query 提取器挡掉，退化为纯函数契约问题。）
- **修法**：`limit.max(1)` 或 handler 处 `q.limit.filter(|l| *l > 0)`。

### BUG-2026-09-13-19 ⬜ P2 畸形 parameter 废弃整个 invoke
- **位置**：`crates/qaqh-gate/src/tool_parser.rs:164`（`extract_attr_value(after_p, "name")?` 的 `?` 逃逸到函数级）
- **触发**：模型输出 `<parameter foo="x">`（漏 name）或属性引号未闭合 → parse_invoke_block 整体 None → 原始 XML 进正文，工具调用无声消失。同文件 L398-401 已有正确写法（unwrap_or_default）。
- **修法**：`let Some(param_name) = ... else { break }`（跳过该参数而非废弃调用）。

### BUG-2026-09-13-20 ⬜ P2 responses sync 空内容 Ok("")
- **位置**：`crates/qaqh-gate/src/responses_api.rs:808-824`
- **机制**：模型仅输出 reasoning/工具调用时 result 为空仍 `Attempt::Ok("")`；另两协议（chat_completions_api.rs:977-979、message_api.rs:982-991）均 Fatal("no content")。compact/标题流程拿到空摘要静默成功，污染压缩后上下文。
- **修法**：`if result.is_empty() { Attempt::Fatal(...) }` 对齐。

### BUG-2026-09-13-21 ⬜ P2 sleep_with_cancel 减法下溢
- **位置**：`crates/qaqh-gate/src/transport.rs:60`（`delay - start.elapsed()`）
- **机制**：while 条件判定与减法之间 elapsed 越过 delay（纳秒窗口，毫秒级轮询每迭代一次）→ Duration Sub 无条件 panic（release 也是）。
- **修法**：`delay.checked_sub(start.elapsed()).unwrap_or(Duration::ZERO)`。

### BUG-2026-09-13-22 ⬜ P2 retry-after 无上限
- **位置**：`crates/qaqh-gate/src/transport.rs:209-227`（秒数与 HTTP-date 两条路径均不封顶）
- **后果**：`retry-after: 999999` → run_with_retry L176/183 直接 sleep，回合挂起数小时（本地退避有 30s 封顶 L125，服务端头路径没有）。
- **修法**：`parse_retry_after` 返回前 `min(上限)`（如 120s 或 5×max_delay）。
- **Codex 参照**：上游同样未封顶（`retry_after.rs:245` TODO(anp)），本地退避硬顶 60s；修复即领先（docs/report/2026-09-13-codex-parity-analysis.md BUG-22 节）。

### BUG-2026-09-13-23 ⬜ P2 ProcessRegistry 驱逐丢 os_pid
- **位置**：`crates/qaqh-workspace/src/process_registry.rs:120-140`（按 started 计时 >600s 驱逐终态条目）
- **后果**：驱逐后 `process kill` 的按 os_pid 清理残留后代路径（L454-471）拿不到 os_pid → 后台任务孤儿孙进程无法经 harness 清理；id 不复用且无墓碑，`process check` 无法区分"已结束"与"id 无效"。
- **修法**：按终态时间驱逐；驱逐前把 os_pid 挪入墓碑表（或 get_info 对 missing id 返回 `"evicted": true`）。

### BUG-2026-09-13-24 ⬜ P2 generate_seed 无碰撞检查
- **位置**：`crates/qaqh-session/src/manager.rs:897-910`（DefaultHasher(nanos+pid) 截断 32 位，无 exists 复查）
- **后果**：碰撞时 persist_new_session 加载旧 meta 覆盖 created_at/cwd，save_append 的 msg_id 去重还会丢新消息——静默写穿旧会话目录。
- **修法**：生成后 `session_dir(seed).is_some()` 重试。
- **Codex 参照**：身份用 UUID（ThreadId）+ OS 写者锁，不玩 hash 截断（docs/report/2026-09-13-codex-parity-analysis.md BUG-24 节）；中期正解为 seed 换 UUID 形态。

### BUG-2026-09-13-25 ⬜ P2 push_image_to_last_user 静默丢图
- **位置**：`crates/qaqh-message/src/store.rs:699-702`（`if let Some(turn) = self.turns.last_mut()` else 无声跳过）
- **触发**：`ingest` 被拒（receipt.stored=false，如 context 摄取拒绝）或 replaying 态 → 无 user turn → 图片既不入 store 也不报错。
- **修法**：返回 bool/记录诊断（与 push_assistant 的 error log 对齐）。

### BUG-2026-09-13-26 ⬜ P2 workspace create TOCTOU
- **位置**：`crates/qaqh-session/src/grouping.rs:195-203`（先无锁查重 L196-203，后加锁 L211）
- **后果**：两线程并发 create 同一路径 → 重复 workspace 条目，左侧筛选失焦（generate_id 的计数器只防 id 碰撞不防重复注册）。
- **修法**：查重挪进锁内。

### BUG-2026-09-13-27 ⬜ P2 空 text 块 400
- **位置**：`crates/qaqh-gate/src/message_api.rs:125-127`（`{"type":"text","text":""}` 兜底）
- **触发**：user 消息所有块被转换忽略（空 Text 上层过滤 L90、其他变体 `_ => {}`）→ 兜底产出空 text 元素 → Anthropic 及严格端点 400 不可重试 → 回合 Fatal。
- **修法**：占位文本（如 `"(empty)"`）或丢弃该消息。

## 附录 A：核对过、确认有防护的方向（不修）

- gate SSE：多字节 UTF-8 跨 chunk、CRLF、[DONE] 缺失残帧 flush、重试整请求无重复事件、全部字节切片均有安全访问。
- sequencer/lease_store（qaqh-runtime/ringing）：saturating 算术、惰性过期、重协商清僵尸身份均有测试锁定。
- daemon 单实例锁（OS 级 try_lock）、discovery 原子写、信号优雅退出、auth Bearer 全串比较、5 个 seed 端点三层校验齐全。
- WAL torn-tail JSON 损坏路径（quarantine + 前缀保留）正确——问题仅在 IO 错误路径（-06）。
- session 存储读侧：torn tail 容错、msg_id 去重、index.jsonl 幂等重放均健壮。

## 附录 B：复核推翻的子代理结论（勿修）

1. **"无 cwd 的 exec 在 Level 3 免审批执行"（子代理报 P0）**：不成立。`lookup_category`（runtime.rs:428-433）是单一事实源，`unwrap_or(Write)` 仅在工具未注册时触发，而未注册工具在 handler 查找处先失败，够不到授权路径。
2. **"serve 账本跨会话互踩"（子代理报 P1）**：影响面存疑。serve 进程按 job 串行（serve.rs:396-403），take_pending 在每个 job 响应前调用，窗口内不积压两会话增量；仅 daemon 侧消费顺序错乱才互踩，属理论竞态。
3. **pow 溢出 release panic（gate 子代理报 P0 panic）**：release（overflow-checks 关）下是回绕非 panic，后果为退避塌缩重试风暴（已按此改写 -02）。

## 附录 C：未覆盖区域（下次扫描候选）

- qaqh-runtime 的 engine_* 大文件族（engine_turn / engine_tool / engine_compact / loop_dispatch_* 全文）
- qaqh-workspace 各工具 handler 细节（exec 进程树 / glob / grep / web）
- qaqh-mcp / qaqh-lsp bridge 全文（子进程生命周期仅抽查）
- qaqh-runtime/src/service.* / host_impl / workspace_supervisor
