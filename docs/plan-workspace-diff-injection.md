# 工作区变更审计注入计划（exec 盲区 → 模型可见 diff）

> 状态：**PR2 设计冻结，chat + responses 两条在用通路已用单测钉死形态**；
> **Anthropic `/v1/messages` 通路未完成，显式挂起**（见 §6 关闭条件）。
> 依据 2026-10-02 代码实证 + 本地端点实测（8317 TraeRelay、8788 anthropic 兼容代理）。
> 关联：`E:\spy\DESIGN.md`（codespy 集成分析）、`docs/spec-file-mutation-delta-v2.md`
> （file_state 归位）、`docs/plan-permission-extraction-v2.md`（无交集，可并行）。

## 0. 问题（实证）

模型用 `exec` 跑 python 脚本批量改文件后，宿主测不到它实际改了什么，模型只能再
`read`/`cat` 自查。四步证据链环环皆断：

| # | 位置 | 断点 |
|---|---|---|
| 1 | `execution.rs:254` → `manager::extract_files_affected` | `permission.rs:110` 对 `exec` 只取 `args.cwd` ⇒ 审计对象 = 一个目录，零文件 |
| 2 | `execution.rs:256 / :394` | `before_sha`/`after_sha` 取 `file_state::last_hash`（工具账本），非磁盘实测；exec 未建基线 ⇒ 恒 None |
| 3 | `journal::record_change` 调用方仅 5 处：`apply_patch.rs:194`、`copy_range.rs:312`、`file_mutate.rs:446`、`edit/handler.rs:238`、`web.rs:186` | `exec/` 整个目录零引用 journal/file_state/record_change ⇒ SMJ 对脚本写完全无感 |
| 4 | `conflict.rs:13` `file_write_paths` 对 exec 返回空 | `tool_runtime.rs:94` 永不把 exec 排进串行 ⇒ exec 必落并行批（4 worker） |

**二次症状**：脚本改过文件后 L3 账本指纹过期，模型下一次 `edit`/`write` 命中
`file_mutate` 的 `stale_file`（`ToolErrorKind::Conflict`），hint 明写
"Use read to obtain current content and hash, then retry"。**当前架构是用"逼模型重新
read"补偿"宿主测不到改动"**——即本计划要消除的对象。

## 1. 状态表

| 项 | 内容 | 状态 |
|---|---|---|
| **PR1** | 新建 `crates/qaqh-spy`（scan/store/report/diff；去 clap CLI / watch / procmon） | **已完成**（`feat(spy)` 提交；11 单测 + 1 e2e 全绿，零新依赖） |
| **PR2** | 批次边界扫描 + `ContextFlow` 注入（chat + responses） | **已完成**（`feat(runtime)` 提交；端到端 2 用例 + message 恢复用例全绿） |
| **PR2b** | `message_api.rs:266` 合并规则扩展（Anthropic 载体吸收后置纯文本）+ 三协议形态测试 | 已入仓（`feat(gate)` 提交；118 全绿、fmt/clippy 干净）；**严格性未经验证 → 端点验证未完成，挂起**（§6） |
| **PR3** | spy 反向喂 `journal::record_change` + `file_state::record_write` + 报告内联回滚命令 | **已完成**（见 §9） |
| **PR4** | `serial_call_ids` 纳入 exec ⇒ per-exec 精细归因 | 未开始（可选） |

## 2. 三协议形态对照

注入 = 一条 `role=user` + `name="workspace"` 的消息，排在**整批 tool 结果之后**。

| 通路 | 产出形态（单测实测） | 约束 | 判定 |
|---|---|---|---|
| Chat Completions | `["user","assistant","tool","tool","user"]`，末条带 `name:"workspace"` | **硬 400**：`assistant(tool_calls)` 之后必须紧跟全部 tool 消息（`chat_completions_api.rs:759-766`） | ✅ 天然合法 |
| Responses | `[message, function_call, function_call, function_call_output, function_call_output, message]` | 无紧邻约束：items 按 `call_id` 各自寻址 | ✅ 天然合法 |
| Anthropic | 现状 `["user","assistant","user","user"]`（连续两条 user）；PR2b 后 `["user","assistant","user"]` + `[tr,tr,text]` | 该通路**并不维持**严格交替（§5.6） | ⚠ 未完成 |

## 3. 关键实证发现

**3.1 注入通路已存在，不用新建。** `agent/injection.rs:50` `Injection`（`role=ROLE_USER`、
`name=Some(source)`）+ `InjectionBus`（command_id/input_id 双幂等、session 作用域、
优先级 Interject/Steer/Normal/Deferred、安全点限额 8/4、compact 期间自动降级）+
`ContextFlow`（`qaqh-message/src/context_flow.rs:229`，`register/submit/ingest`，
`IngestTraceEntry.outcome ∈ pending|stored|deduped|skipped|rejected`）。
头注释已预留扩展位："subagent reports today; system/MCP in the future"。

**3.2 顺序保证是结构性的，不是需要我构造的。** `loop_outcome.rs:545-556`
`Outcome::ContinueTurn` 分支注释原文：*"工具调用回合结束 → 下一轮 gate 前，消费
cmd_rx 中排队的 as_system 注入进入总线，再由 ContextFlow 在 lap 边界落盘，使下一轮
LLM 请求立即可见"*，随后 `drain_pending_injections(); drain_injections();`。
即 subagent 报告今天走的就是"全部 tool 结果之后"这个位置。

**3.3 Chat 的 400 约束已有先例解法。** `convert_messages` 用 `pending_media` 缓冲
（`:766-770`、`:920`）把图片合成消息推迟到整串 tool 消息走完。但该保护**只覆盖合成
媒体消息，不覆盖历史里真实的 user 消息**——见测试
`chat_does_not_reorder_a_user_message_wedged_between_tool_messages`。

**3.4 两协议不对称。** Responses 允许 message item 插在 `function_call_output` 之间
（图片降级就这么做），Chat 不允许。已用
`responses_allow_message_item_between_function_call_outputs` 锁定，防日后误按 Chat
约束去"修"它。

**3.5 `name` 只在 Chat 活到 wire。** `chat_completions_api.rs` 的 user 分支显式
`obj["name"] = json!(n)`；Responses 的 message item 与 Anthropic 均不携带 `name`。
⇒ **文本自标签是承重件**，不是装饰。

**3.6 严格交替从来不是本仓维持的不变式。** guard 测试
`plain_user_before_tool_results_is_not_merged_into_the_carrier` 通过即证明：
"前置纯 user + 后续 tool_result"这条**既有路径今天就产出连续两条 user**，且
`consecutive_user_messages_are_merged`（`message_api.rs:1310`）主动锁死该行为。
⇒ PR2b 定位从"防 400 阻塞项"降级为**加固项**。

**3.7 已知分叉：落盘 ≠ 传输。** `engine_turn.rs:53` 点名"trailing 注入已持久化但
模型请求未携带"。这是 PR2 的真闸门，与协议无关。

**3.8 新发现：trailing 注入判定硬编码 `"subagent"` 字面量（PR2 实施中抓出）。**
`MessageStore` 有四处各自复制同一契约——`push_trailing_system` 的 `debug_assert`、
`from_messages` 的 user 分支、中段 `_` 分支、以及 Environment 锚点的"第一条真实
用户消息"判定。injection.rs 头注释宣称的"新 source 只需声明自己"在 store 层**并不
成立**：未登记的新注入源会在崩溃恢复时被误认成真实用户输入（虚增回合数、把报告
钉进对话史）。已收敛为 `USER_INJECTION_SOURCES` + `is_user_injection` 共享谓词，
`workspace` 登记在册，并由 `from_messages_restores_workspace_injection_as_trailing`
锁定。

实施教训：站点 4 原本是**不限角色**的 `[SUBAGENT ` 前缀回退（`push_system_input` 造
的是 system 角色 + 该前缀），把它收紧成 `role==user` 会静默丢弃旧档——该回归被
`cargo test -p qaqh-message` 当场抓出。前缀回退的适用范围本身是契约的一部分。

## 4. 设计决策（冻结）

| # | 决策 | 依据 |
|---|---|---|
| D1 | **每批一条注入**，内部按 `call_id` 分组，排在整批 tool 结果之后 | Chat 硬 400 约束（§3.3）+ 并行 exec 归因污染（§0.4）+ Steer 安全点限额 |
| D2 | `input_purpose = Steer`；**禁用 `TriggerTurn`/`handle_system_input`** | 后者会 `ctx.cancel.clear()` 开新回合，`engine_input.rs:268-272` 警告会复活已取消会话 |
| D3 | 新 `ContextSource` `builtin::WORKSPACE`（`name="workspace"`），TurnBoundary-timed，`visibility().context=true`，`dedupe_key = scan_id` | 非 TurnBoundary 触发 `FlowError::TimingMismatch`；`context=false` 会被 `skipped`（模型看不到）；key 带 scan_id 才不被相邻去重误吞 |
| D4 | 文本自带标签 `[workspace-changes call=<id> scan=<id>]`，⚠ 置顶 + 内联回滚命令 | §3.5——脱离 ToolResult 后 wire 上唯一来源标识 |
| D5 | 报告 ≤ 4KB，每文件一行 stat，仅最可疑 K 个文件展开 5 前/3 后 diff | 防上下文爆炸；与 `EXEC_CHAR_LIMIT=8000` 解耦（本方案不进 fold） |
| D6 | **canonical `ToolResult` 零改动**，`execution.rs` 不动 | `tool_result.rs:257` "绝不进入模型投影" 契约 + `store.rs:2326` 测试锁死；且免掉"必须在 fold 之后追加"的隐藏顺序依赖 |

## 5. 已入仓测试（5 条，`cargo test -p qaqh-gate --lib` 118 passed）

| 测试 | 位置 | 钉住什么 |
|---|---|---|
| `workspace_diff_injection_after_tool_run_keeps_pairing_and_name` | `chat_completions_api.rs` | Chat 形态合法 + `tool_call_id` 配对未动 + `name` 上 wire |
| `chat_does_not_reorder_a_user_message_wedged_between_tool_messages` | `chat_completions_api.rs` | 转换器不救插队 ⇒ flush 时机须由调用方保证 |
| `workspace_diff_injection_follows_function_call_outputs` | `responses_api.rs` | Responses item 顺序 + `name` 缺席（自标签承重） |
| `responses_allow_message_item_between_function_call_outputs` | `responses_api.rs` | 与 Chat 的不对称 |
| `workspace_diff_injection_is_absorbed_by_tool_result_carrier` + `plain_user_before_tool_results_is_not_merged_into_the_carrier` | `message_api.rs` | PR2b 目标形态 + 被保护 case（§3.6 证据来源） |

## 6. Anthropic `/v1/messages` 通路：未完成项与关闭条件

**已改**：`message_api.rs:266` 一行合并条件

```rust
if last_is_tool == cur_is_tool || (last_is_tool && !cur_is_tool) {
```

吸收后由既有稳定分区（`:303-318`）保证 `[tool_result, tool_result, text]` 顺序——
该顺序在 `parallel_tool_result_images_merge_into_one_user_turn` 注释里记为
"实测 `[tr,tr,img,img]` → 200"。

**为什么仍算未完成**：端点实验**无判别力**。

| 探测 | 结果 |
|---|---|
| `127.0.0.1:8317`（TraeRelay.exe） | **无 `/v1/messages`**：二进制内 `anthropic`/`claude`/`/v1/messages` 关键字命中数全为 0；路由仅 `/v1/chat/completions`、`/v1/responses`、`/v1/models`、`/v1/status` |
| `127.0.0.1:8788/v1`（从 `~/.qaqh/config.toml` 顺出） | 真 Anthropic 形态，但为 anthropic→openai 宽松转换器（id 前缀 `msg_chatcmpl-`，`model:"auto"`） |
| 负对照：交错 `[tr,text,tr]`（`message_api.rs:298-300` 记为真实 400 的形态） | 8788 上 `qwen3.8-flash` 与 `claude-sonnet-4-5` **均返回 200** |
| Chat / Responses 交错形态 | 8317 + 8788 均 200 |

⇒ 全 200 无法证伪也无法证实。**PR2b 的前提（"不合并会被 Anthropic 拒"）目前只有仓内
注释一条二手证据，且已被 §3.6 削弱。**

**关闭本项需要下列之一**：
1. 真实 Anthropic key 或会做形态校验的兼容端点（如注释点名的 opencode zen），把
   §2 表里三种形态各打一次，拿到 400/200 差分；
2. 或明确接受"以 qaqh 自身产出形态为准"，把 PR2b 归档为加固项，不再追端点证据。

**当前不在关键路径**：`~/.qaqh/config.toml` 的 `profiles.default.endpoint = "openai"`，
Anthropic 通路今天没在用。

**回滚成本**：revert 那一行条件即可，两条相关测试随之失败（自锁，不会留静默分叉）。

**附带待办**：合并进载体后 `name="workspace"` 在 Anthropic wire 上丢失（该协议无
`name` 字段），来源标识只剩文本标签——D4 因此是硬依赖而非建议。

## 7. 下一步

1. ~~**PR1**：建 `crates/qaqh-spy`~~ **已完成**——零新第三方依赖（`Cargo.lock` 仅 +15 行
   新 package stanza）；`jiff` 已换 `chrono`（与 `execution.rs` 审计时间戳同惯例）；
   存储根 `QAQH_SPY_DIR` ∨ `platform::data_dir()/spy/<工作区哈希>`。
   遗留：`report.rs` 只出危险启发式、**不含可执行回滚命令**（codespy README 宣称有，
   代码里没有）→ 并入 PR3 接到 `journal replay` / 未来的 `spy undo`。
2. ~~**PR2 接线**~~ **已完成**，但落点从计划的 `execute_batch` 上移到
   `turn_lap/admit.rs::execute_admitted_batch`——`execute_batch` 内部有多个取消早退
   分支，只有包裹层能覆盖全部退出路径。`execution.rs` 一行未动（D6 兑现）。
   提交时立即 `drain_turn_boundary`：`Loop::drain_injections` 在总线无投递时提前
   返回、不排空 `ContextFlow::pending`，只 submit 会让报告永不落盘（§3.7 反向形态）。
3. **落盘断言已入仓**（`qaqh-runtime/tests/workspace_change_injection.rs` 断言注入
   进了 message store 且排在 tool 结果之后）。**仍缺**：下一次 provider 请求视图
   确实携带该文本块的端到端断言——目前由 gate 侧形态测试间接覆盖，未串成一条链。
4. ~~**PR3**~~ **已完成**，细节与偏差见 §9。
5. **待办**：`QAQH_SPY_DISABLED` 目前是唯一开关；接入 `config.toml` 的正式配置项
   尚未做（灰度期用 env 足够，长期应进 `profiles` 段）。

## 8. 边界备忘

- `journal.rs:96 store_blob(content: &str)` / `:111 read_to_string` ⇒ SMJ **只存文本**；
  spy 存 `Vec<u8>`。二进制/非 UTF-8 脚本产物 SMJ 接不住。
- hash 域不同：`file_shared.rs:13 content_hash = sha256(LF-canonical 字符串)`
  vs spy `sha256(原始字节)`。**不得混填同一字段**（会污染 `expected_hash` 防漂移校验）。
- 两套 CAS 第一阶段并存（`data_dir()/journal/blobs` 与 `data_dir()/spy/objects`）：
  SMJ 无全量 manifest ⇒ 给不出"时刻 T 完整状态"，也无法发现未声明的写。
- 性能基线（spy 实测）：qaqh-backend 738 文件首扫 0.9s，稳态 66ms ⇒ 每批两次扫描
  ≈ +130ms，可接受。


## 9. PR3 实施记录（与计划的偏差）

回填发生在 `workspace_audit::backfill`，位于「批尾扫描」与「渲染报告」**之间**——
这个顺序是被需求逼出来的：回滚命令需要 SMJ 序号，而序号只有先记 SMJ 才拿得到。
因此 qaqh-spy 的 `report_since` 被拆成 `changes_after_tool_end` + `render_report` 两步
（`report_since` 保留为二者的组合，既有 e2e 测试不受影响）。

### 9.1 三条回填契约（均有测试）

| 契约 | 实现 | 测试 |
|---|---|---|
| SMJ 记上，模型能用已有 `journal` 工具 query/replay | `journal::record_change`，`op` 取 `scan_added`/`scan_modified`/`scan_deleted` | `self_declared_write_is_not_double_recorded_in_smj` 反向覆盖；主用例断言 `step.op == "scan_added"` |
| 账本指纹刷新 ⇒ `stale_file` 假阳性消失 | `file_state::record_write` / `record_delete`，路径**必须先解析成绝对路径** | 主用例断言 `file_state::last_hash(abs).is_some()` |
| 报告带可执行回滚命令 | `journal action=replay file=<p> at=<seq-1> out=<p>` | 主用例断言含该命令，并实调 `replay_to_path` 验证文件真被还原/删除 |

`at = seq - 1` 对三种变更都正确：replay 到本步之前即撤销本步——新增文件此前不存在
⇒ `replay` 返回 None ⇒ 删除；删除文件 ⇒ 取回旧内容；修改 ⇒ 回到改前。

**这条命令本身也被验证过可执行**，不是发出去就算数：测试直接调用 journal 工具背后
的同一个函数 `replay_to_path(file, Some(seq-1), Some(path))`，断言新增文件确实被删除
（`replay_to_path` 在内容为 None 时走 `remove_file` 分支）。此前仓里有过「README 宣称
有回滚命令、实现里根本没有」的先例，所以凡是发给模型的命令都要有对应断言。

### 9.2 去重（计划里没有，实施中必须加）

`write`/`edit`/`apply_patch`/`copy_range`/`web_fetch` **自己已经调过 `record_change`**。
spy 若不加区别地补记，`journal query` 会出现成对假记录、审计流水翻倍。因此 `begin`
捕获本批的 `declared_writes`（取自 `conflict::file_write_paths`，按
`file_state::ledger_key` 归一化），`backfill` 跳过落在该集合里的路径。

`exec` 不在 `file_write_paths` 的名单里（它返回空集），所以脚本副作用天然落不进该
集合——正是我们要补的那一类。这一点是去重规则能成立的关键，测试里用注册成 `file`
工具名的自报探针把它钉住。

**已知偏差**：同一批里若某路径既被工具自报、又被脚本二次改写，SMJ 只会留下工具自报
那一条，脚本那次后续写入在 SMJ 里不可见（报告仍然会显示，因为报告来自状态扫描）。
要彻底消除需按调用切分扫描，即 §1 里挂起的 PR4。

### 9.3 二进制跳过

SMJ 的 `store_blob(content: &str)` / `read_blob → read_to_string` 只存文本；喂二进制
会在 replay 时才炸。`backfill` 因此对任一侧非 UTF-8 的变更**整体跳过**（不记 SMJ、
不刷账本），并在报告尾部如实说明数量。快照本身仍在 spy 的 CAS 存储里，字节级可取回。

### 9.4 账本刷新与 SMJ 解耦

`record_change` 返回 None 表示 SMJ 写失败，但磁盘内容已经是新的——指纹**必须**跟上，
否则 `stale_file` 假阳性照旧。所以账本刷新不受 SMJ 成败影响，只有回滚提示依赖 seq。

### 9.5 验证

- `cargo test -p qaqh-runtime`：全部通过，含既有 harness
  （`tool_ordering_contract` / `tool_ledger_*` / `cancel_keeps_tool_results`）——
  证明钩子在每次批上跑不会污染它们对消息序列的断言。
- `qaqh-spy` 12、`qaqh-message` 65、`qaqh-gate` 118 全绿；fmt/clippy 净。
- `workspace_change_injection` 3 用例：注入+落盘+排序、零变更不注入、自报路径不双记。