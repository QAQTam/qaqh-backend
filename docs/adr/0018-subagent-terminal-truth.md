# ADR-0018 子代理终态说真话：掐流=失败、预算与父同律、配置面退役

状态: `已采纳` · 决策日期 2026-11-13 · 决策人 用户提议 + QAQ agent（宪法第 3/4/5/9 条定稿）

## 背景（已核对事实）

用户报告：子代理总是在一次模型调用后"消亡"，管理面却提示 finished，无任何报错。
排查证实**四处缺陷串联**，把失败洗成完成：

1. **管理面投影不看 outcome**：`crates/qaqh-session/src/db/view.rs`
   `child_terminal_status` 把回合 `status='finished'` 一律投影成
   `SubagentTerminalStatus::Completed`。而 loop 的三种终态
   （Completed/Cancelled/Failed）都落 `status='finished'`
   （`session_loop.rs` `finish_completed/finish_cancelled/finish_failed`），
   区分只在 `outcome_json.terminal`。契约里 `failed` 变体从未被发射。
   反证：新测试先红——失败子代理投影出 `Some(Completed)`。
2. **120s 墙钟断头台**：daemon 每 60s 扫描
   （`qaqh-daemon/src/server.rs`），取消回合时钟 ≥
   `config.subagent.timeout_secs`（缺省 120）的子会话回合
   （`registry/lifecycle.rs::cancel_expired_subagent_turns`）。
   取消终态按 T-1-2 不注入正文 → 父会话收到空答 cancelled，管理面又投影成
   Completed。父会话没有这道闸——父的预算语义是上下文压力 → handoff。
   这是"自动消亡无报错"的主因，也是子代理与父代理唯一的独立预算。
3. **gate 掐流洗白（partial-keep）**：四个 wire 驱动在上游中途掐流且已有
   产出时，用半截缓冲伪造 `Done{stop_reason:None}` 并返回 `Outcome::Completed`
   （chat_sdk/messages_sdk/gemini_sdk/responses_sdk）。注释声称"交由上层
   续写机制接管"——**该机制从未存在**：`session_loop` 对 `Unknown` finish
   直接 `finish_failed("provider_incomplete")`，真实 HTTP 原因只进日志。
4. **SDK EOF 合成终态被当真终态**：mutil-ai 在流 EOF 未收到真终态帧时合成
   `Done`（`finish_reason=None`，`raw` = SSE chunk 数组）。Responses 驱动
   旧映射 `None if raw.is_array() => None`（靠 runtime Unknown 兜底）、
   `None => Some("stop")`（retain_raw=false 时**直接把掐流洗成 completed**）。
   Chat/Anthropic/Gemini 驱动把合成 Done 当真终态收口（`responded=true`）。

另：子代理模型选择本就继承父会话行（`registry/spawn.rs` child_selection），
catalog `max_output_tokens` 父子同源——**不存在独立 max_tokens 上限**；
用户要求的"放宽/对齐 maxtoken 语义"落点就是第 2 条的独立墙钟预算。
子代理工具面则被 `cfg.subagent.default_tools`（缺省 `["read","exec_command"]`）
单独收窄，与父会话 STANDARD 不同律。

## 决策

1. **终态投影读 outcome 权威**：`child_terminal_status` 读
   `status + outcome_json`，`Finished{terminal}` → Completed/Failed/Cancelled，
   `Interrupted` → Cancelled；结算行缺 outcome 即 `Corrupt`（与 `turn_view`
   同律 fail-closed）。wire 形状零变化（`SubagentTerminalStatus` 四变体早已
   冻结在契约），只是值说真话。
2. **掐流=显式失败**：gate 只在观察到 provider 真终态时返回 Ok。
   - Chat/Anthropic/Gemini：`Done.finish_reason` 缺席 = SDK EOF 合成收口
     → 记 `interrupted` 走失败出口（真终态恒带 finish_reason）。
   - Responses：判别式 = `raw.is_array()`（合成收口是 chunk 数组，真
     completed 是 response 对象；真 completed 的 finish_reason 只携带
     `incomplete_details.reason`，缺席映射 "stop"）。`roomy_response_limits`
     钉死 `retain_raw=true` 保判别式恒有效。
   - 失败出口统一：有产出 → `Outcome::Fatal(真实原因)`（不重试——半截已流出，
     整请求重试会重复 UI 输出）；零产出 → `EmptyStream` 整请求重试（无损）。
     runtime 既有 `StartedUnknown → seal_partial → finish_failed("provider_failed")`
     链路保留部分文本并把真实原因写进 `TurnError`，终态回传与 SubagentView
     随之说真话。删除 responses 的 `partial_done` 死代码。
3. **砍掉子代理独立墙钟预算**：`cancel_expired_subagent_turns`
   （registry + service）、daemon 扫描块、`SessionLoop::turn_elapsed_secs`
   与 `ActiveTurn.started_ms` 全退。子代理=会话（ADR-0014），回合预算与父
   同律：上下文压力 → handoff 逃生阀。树深（1）与消息配额（16/1024）保留为
   **内核常量**（registry 缺省），测试/运维钩子 `set_max_depth` /
   `set_message_quota_limits` 保留。
4. **前端子代理配置面退役**：`ConfigDto.subagent` / `ConfigPatch.subagent`
   （`SubagentDto`/`SubagentPatch`）、引擎 `SubagentConfig`、持久层
   `PersistentSubagentConfig` 全删。spawn 工具不再读全局 config：
   子代理工具面 = 空白名单 = STANDARD，与父会话同律。
   内部 `ToolEffect::SubagentSpawned` 的 `timeout_secs`/`spawn_timeout_secs`
   同删（冻结的 `ToolEffectInfo::SubagentSpawned` 本就只三字段，零变化）。
5. **存量 config 一次性迁移**：`[subagent]` 段由 daemon 历次 save 自写，
   存量安装人人都有；`deny_unknown_fields` 会因此拒启动。`ConfigStore::load`
   读入口剥离该段并原子写回（与工具名迁移写回同律），此后磁盘永不再现；
   其余未知键仍 fail-closed。这是数据迁移，不是兼容层。

## 被否选项

- **保留 partial-keep，补一个"续写机制"**：为半截响应发明带词重启等于把
  上游故障伪装成正常回合续写，且已流出的工具参数可能半截——续写会重复执行
  副作用。失败就是失败，让模型/用户看见原因（宪法 5/9）。
- **保留 120s 超时但调大/可配置**：独立墙钟预算与"子代理=会话"正面冲突，
  是 ADR-0014 收敛后残留的第二权威。父没有的闸子也不该有；上下文压力已有
  handoff 逃生阀，无需第二套预算机构。
- **per-spawn deadline 列**：ADR-0014 已否（为悬置功能加 schema 迁移）；
  本轮连 config 驱动扫描一并退役，更无加列理由。
- **ConfigDto 保留 subagent 段只读/恒默认**：死字段是兼容套娃（宪法 4）。
  读模型无向前兼容是本仓既定纪律（前后端共进退），删除走 refreeze 账本。
- **改 mutil-ai 让它 EOF 不合成 Done**：SDK 是姊妹仓独立版本线，合成收口
  对其它消费者可能是合理行为；判别式在 gate 侧收口，不跨仓改契约。

## 后果

1. **契约分类（宪法 15）**：
   - `ConfigDto`/`ConfigPatch` 删 `subagent` 段 + `SubagentDto`/`SubagentPatch`
     两型删除 ⇒ **wire 实变（字段/类型删除）= `[contract-refreeze]`**，
     记入 ADR-0010 攒着的清单，切 v3.1 时一次吸收；`docs/api-surface/v3.0`
     一字节不动（规则 16），main 上 `check --against` 红为设计状态。
   - `SubagentView.status` 值域说真话、gate 失败语义、超时退役、内部
     ToolEffect 字段 ⇒ 内核行为/非冻结面，**导出物零变化**。
   - `SESSION_CONTRACT_REVISION` / `COMMAND_CONTRACT_REVISION` 不 bump
     （命令面零新增；读模型字段删除的代价归 ADR-0010 §待决 1 统一裁决，
     与 Goal 段新增同律：ConfigDto 无向前兼容，前后端共进退）。
2. **前端连带**（姊妹仓）：设置页删除子代理段（timeout/defaultTools/
   maxDepth/消息配额）；Team 页现在会看见 `failed`/`cancelled` 终态，
   需要对应文案；旧客户端读新 ConfigDto 缺 `subagent` 字段——方向相反：
   新客户端读旧 daemon 的多余 `subagent` 键被 serde 忽略（读模型无
   deny_unknown_fields），但新 daemon 的 DTO 缺字段旧客户端解析失败——
   前后端必须同批发布（与 Goal 段落地同律）。
3. **行为变化**：长任务子代理不再被 120s 静默取消；上游掐流的子代理终态
   为 Failed 且携带真实原因（父会话回传消息 `failed (exit=1)` + 原因正文）；
   子代理工具面从缺省 read+exec_command 放宽到与父会话同律的 STANDARD。
4. **预存缺陷（与本决策无关，验证时挖出）**：
   - `qaqh-gate` 的 6 个 anthropic 测试在 HEAD 即红（thinking budget 校验：
     4096 < 5120，effort high）——`7c08ed3c` 把静默 clamp 改硬 ensure! 时
     未同步夹具；本轮 diff 未触碰该校验（`git diff` 实证），留待其所有者处置。
   - `visualize` 工具（`4661a62a`）`default_timeout=ZERO` 而能力表条目
     非 interactive → `register_typed` 描述符校验 panic，**新二进制无法
     装配任何会话**（15 个 agent-runtime 测试同因此红）。这是纯代码错误
     （宪法 5），本轮顺手改正：30s 快工具超时（visualize.rs 一行）。

## 落地清单

- [x] 终态投影读 outcome（view.rs）+ 反证测试
      `subagent_terminal_status_reflects_turn_outcome_not_just_status`
      （先红后绿；qaqh-session 31 passed）。
- [x] 四 wire 驱动掐流=失败 + SDK 合成终态判别 + retain_raw 钉死 +
      `partial_done` 删除；旧钉死测试重写为
      `responses_stream_without_terminal_event_is_a_failure_not_a_salvaged_done`。
- [x] 超时清扫全链退役（lifecycle/service/daemon/turn_elapsed_secs/started_ms）。
- [x] 配置面退役（config-api/config/types/dto）+ spawn 工具面与父同律 +
      内部 ToolEffect 超时字段删除。
- [x] 存量 `[subagent]` 一次性迁移 + 回归测试
      `subagent_section_migration.rs`（剥离写回/重载/save 不复活/未知键仍拒）。
- [x] ADR-0010 账本登记；ADR-0014 决策 4 标注被本 ADR 取代。
- [x] 定向验证（`cargo test -j 2`）：qaqh-session 31 绿（含反证闸门）；
      qaqh-gate lib 99 绿 + response_stream_limits 2 绿 + gate_test 42 绿
      （6 个 anthropic 预存红，见 §后果 4）；config/config-api/types/
      tool-core/subagent 全绿（含迁移回归）；agent-runtime 37 绿；
      runtime/daemon 全绿；workspace 129 绿（4 个授权/exec 环境性预存红，
      本轮 workspace diff 仅 visualize 一行）。
- [x] 契约导出核对（`qaqh-contract-export check --against docs/api-surface/v3.0`，
      本机实测）：文件集 610 vs 608，only-in-first 恰为 ADR-0017 已记账的
      6 个 ExecSessionOutput* 产物，only-in-second 恰为本条删除的 4 文件
      （`SubagentDto`/`SubagentPatch` × ts/cs）。main 上红为设计状态
      （ADR-0010 §背景 2，report-only）；金标准一字节未动。
      导出器类型清单同步删除两型（main.rs），夹具由 ConfigDto::default()
      机器生成自动跟随。
- [x] **并发留痕（宪规 19）**：实施期间另一会话在共享树写入 AssistantBlock
      `ord` 权威排序特性（state.rs/store.rs/owner.rs/owner_tests.rs +
      session_loop.rs 的 `ord: 0` 回填，16:57 落盘）。两线正交但同文件
      （session_loop.rs）：本提交经部分暂存只取超时退役的 3 个删除 hunk，
      ord 改动原样留工作区由其所有者提交。组合树验证绿（agent-runtime 37 /
      runtime / daemon 全绿即含双方改动）；本提交子集在 ord 落盘前已编译验证。
