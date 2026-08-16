# PLAN：注入 / 重放 / 缓存命中一致性修复

> 状态：**实施中**——用户已拍板 §7 决策（D1 退役 journal / D2 静默 drop / D3 折叠 / 角色统一 user），核心改动已落地（2026-08-15）：A1 折叠、B1 退役 journal、角色统一；A2 经实证判非必需（写入序合并已保留位置）。D4/D5/D6 待拍板。
> 日期：2026-08-15
> 范围：deepx-message（store/context_flow）、deepx-msglp（lifecycle/agent/loop_core/engine_compact/engine_input）、deepx-session（manager journal）、deepx-daemon（ringing_http 注入入口）、deepx-workspace（runtime::all_tools）。不含 WinUI。
> 证据基准：本 PLAN 所有 file:line 均经直接代码阅读验证（2026-08-15，subagent 报告交叉印证）。

---

## 0. 目标与原则

**目标**：崩溃/重启/compact 之后，模型上下文与崩溃前**字节级一致**；注入有生命周期（注入 → 消费 → 历史 → 可折叠），**不是永驻状态**；工具 schema 纳入缓存命中计算与落盘；**唯一允许破坏缓存前缀的操作 = 创建新 session 与 compact**（compact 视为新 session）。

**原则映射**（用户指令 → 设计约束）：

| # | 用户指令 | 设计约束 |
|---|---------|---------|
| P1 | 上下文至始至终如一 | 注入 = 消息流事件，恢复 = 纯回放，不删、不重排、不重放 |
| P2 | 删除重放机制 | 退役 injections.jsonl 崩溃窗口重放；resume 不再移除系统消息 |
| P3 | 恢复时静默 drop | 崩溃窗口未消费注入直接丢弃（逻辑见 §4.2） |
| P4 | 仅状态变更插入新 user 提示 | 注入/恢复/工具更新等状态变化一律以 user 提示在**当前位置**追加，不进 system 前缀 |
| P5 | schema 纳入缓存命中与落盘 | 旧 schema 不移除（历史消息不可变）；按工具类别在最新处注入增量；仅新 session / compact 重拉全量 |

---

## 1. 问题现象

1. **注入误放/错误重放**：程序或 workspace 崩溃后恢复服务时，`[SUBAGENT]` 报告被错误恢复——先前已注入并消费的报告（甚至 100 轮前）在**压缩后或崩溃后重新出现**，被模型当作新鲜输入重读。
2. **resume 前缀移除 → 缓存命中归零**：恢复时删除 skill 信封/目录系统消息，系统前缀字节变化，provider 前缀缓存（Anthropic/OpenAI 自动缓存，无显式 cache_control）全量 miss。
3. **注入系统消息恢复错位**：重启后被恢复到"顶层消息"而非按顺序回放（compact 场景最严重）。
4. **schema 未纳入缓存命中**：工具 schema 无 per-session 持久化、无版本；版本升级/会话间 tool_mode 差异静默改变请求前缀；PrefixShape 诊断是内存态，重启即失效。

---

## 2. 根因（代码证据）

### 2.1 注入永驻 + compact 保留旧 id → 错位（现象 1、3）

- **永驻是 by-design**：`store.rs` L333-340 `trailing_messages` 文档明言 "Written exactly once at arrival; never re-injected, never removed (undo=Keep, compact=Preserved)"；`push_trailing_system`（L561-597）文本幂等 + `save_msg` 分配写入序 id。
- **apply_compact 不折叠 trailing**：`store.rs` L1462-1494——`trailing_messages` 完全不动（**保留旧 msg_id**），摘要 turn 取 `first_compacted_id`（最早被压缩 turn 的 id，L1481-1483）。
- **flat_in_write_order 按 id 排序**：L906-927。结果：100 轮前的注入（id 介于摘要与保留 turn 之间）落在**摘要之后、活跃 turn 之前**——"顶层"错位；且摘要输入（`engine_compact.rs` L101 `build_context_for_gate(&[])`）已含该注入文本 → **双重表示**。
- **恢复时 re-push 重编号**：`from_messages`（store.rs L1209-1325）把 `[SUBAGENT`（user 角色 L1264-1265；任意角色 L1315-1321）与 `developer` 消息经 `push_trailing_system` 重放；`save_msg` 在 replaying 期间**重新分配新 id**（L451-463，不落盘）→ 注入在恢复后的 store 里"复活"，继续永驻。

### 2.2 崩溃窗口重放（现象 1 的另一机制）

- `manager.rs` L565-630：`injections.jsonl` append-only journal（`enqueued`/`committed` 事件），at-least-once + store 文本幂等 = "恰好一次"。
- 写入方：daemon `ringing_http.rs` L1722（enqueued，ACK 前）；`engine_input.rs` L274（committed，注入落盘后）。
- 消费方：`loop_core.rs` L692-750 `replay_pending_injections`——worker 启动时重放未 committed 注入。
- **边界缺陷**：注入落盘（messages.jsonl）与 committed 标记写入之间崩溃 → 已消费的注入在重启后**再次注入**；文本幂等只防同 store 内重复，注入被 compact 折叠出活跃视图后幂等失效 → 真重复。此外即便不重复，恢复路径本身把历史注入重新"激活"为新鲜消息。

### 2.3 resume 前缀移除（现象 2）

- `lifecycle.rs` L113-118：resume 时 `remove_system_messages_by_prefix(deepx_skills::ACTIVATION_MARKER)` + `("Available skills")`——删除历史中的 skill 信封/目录系统消息。
- 意图（L110-112 注释）："Old protected skill/catalog system messages must not reactivate instructions by surviving in message history"。
- **手段错误**：技能状态已从 typed metadata 恢复（L127 `agent.skills.restore`），激活集变化由 epoch 机制增量注入（`agent.rs` L463-484 `sync_skill_injection`，epoch 不变不注入）——**不需要删消息**。删除 = 系统前缀字节变化 = provider 前缀缓存全 miss。

### 2.4 schema 不落盘、不参与恢复（现象 4）

- `lifecycle.rs` L129 resume 热载 `tool_defs = runtime::all_tools()`；`agent.rs` L317/L340/L354 各路径运行时重建——**无 per-session 快照、无版本号**。
- 请求链路：`engine_turn.rs` L1285 `tools = Some(tool_defs.clone())` → L1320 `chat_stream` → gate `responses.rs` `convert_tools` → body `tools` 字段。tools 是 provider 缓存前缀的一部分，任何字节变化 = 全量 miss。
- 内部诊断已存在但**内存态**：`agent.rs` L63-84 `PrefixShape::capture`（system_hash/tools_hash/逐消息 hash）+ L135 `prev_prefix` + L419 `if !prev_prefix.system_hash.is_empty()`——重启即空 → 恢复后首次请求的缓存破坏**静默无诊断**。
- `prepared_request_key`（L268-274）含 tools，仅内存 token 校准用，不落盘。
- **已验证**：`openai.rs` L644-700 `convert_messages` 逐消息重建 JSON——`msg_id` **不进 provider wire**（`message.rs` L103-104 虽可序列化，但 gate 转换层剥除）。恢复重编号不影响 wire 字节，排序一致则字节一致。

---

## 3. 设计

### 3.1 A 组：注入生命周期（删除永驻，随消息流折叠）

- **A1 `apply_compact` 折叠 trailing**：压缩时按 msg_id 区间过滤——折叠区内的注入随旧 turn 一起丢弃（其文本已进入 compact 摘要输入，`engine_compact.rs` L101 用 `build_context_for_gate(&[])` 含 trailing）；保留区内的注入留在原位置。`trailing_messages` 语义从 `compact=Preserved` 改为 `compact=Fold`（与 turn 一致）。
- **A2 `from_messages` 保留原 msg_id**：replaying 期间 `save_msg` 不再重新编号（返回原 `msg.msg_id`），`next_msg_id = max(全量 msg_id)+1`（现仅 leading-system 段更新 L1239-1241）→ 恢复后 `flat_in_write_order` 与崩溃前**完全一致**，`[SUBAGENT]`/developer 消息按原位置成为历史，不再"复活"。wire 字节与崩溃前逐字节相同（msg_id 不进 wire，已验证）。
- **A3 语义文档更新**：`push_trailing_system`/`trailing_messages` 注释从 "never removed" 改为 "注入后由模型消费（ack）成为历史；compact 随消息流折叠"。

### 3.2 B 组：恢复语义（删除重放 + 静默 drop）

- **B1 退役 injections.jsonl**：删除 `replay_pending_injections`（loop_core.rs L692-750）+ `mark_injections_committed`（L754+）+ journal API（manager.rs L565-630）+ 调用点（daemon `ringing_http.rs` L1722 enqueued 写入、`engine_input.rs` L274 committed 写入）+ 相关测试（manager.rs L819-862、session_lifecycle.rs L526/L583）。
- **B2 删除 resume 前缀移除**：移除 lifecycle.rs L113-118 两处 `remove_system_messages_by_prefix` 调用。系统前缀保持字节一致；技能指令恢复走 typed metadata（L127）+ epoch 增量注入（agent.rs L463-484）。
- **B3 崩溃检测 + 状态变更提示**：meta 增加 `clean_shutdown` 标记（daemon stop 协议正常写；崩溃/强杀 = 无标记）。resume 检测到崩溃 → 以 **user 提示**在当前位置插入："上次会话因崩溃中断，已恢复；未完成的注入已丢弃。"（P4 范式落地）。
- **B4（收紧）**：`remove_system_messages_by_prefix` 全仓仅 lifecycle.rs L115/L118 两个调用点（2026-08-15 rg 验证）——B2 后成死代码，**删除函数 + 相关测试**。

### 3.3 C 组：缓存一致性（schema 落盘 + 版本化增量）

- **C1 schema 快照持久化**：per-session 落盘 `tool_defs` 快照（meta.json 或独立 `tools.jsonl`）：`{app_version, schema_epoch, fingerprint, tool_defs[]}`。写入时机：会话创建、apply_tool_mode、compact 后。
- **C2 resume 指纹校验**：
  - 快照指纹 == 当前 `all_tools()` 指纹 → 复用快照（`tool_defs` 字节不变 → 前缀缓存可命中）。
  - 不同（版本升级）→ **不移除旧 schema**（历史请求已含旧 tools，消息流不可变）；在**最新处**按**工具类别**注入增量说明（user 提示或 trailing developer）："工具集已更新：新增 X、变更 Y（签名差异）、移除 Z，请按新签名调用"——与 P5 一致。
- **C3 PrefixShape 落盘**：`prev_prefix` 持久化（session 级），恢复后首次 `build_context` 能对比并产出缓存诊断（不再静默）。粒度见决策 D5。
- **C4 重拉时机收窄**：全量 schema 只在**新 session**（lifecycle.rs L194/L230）与 **compact 后**重新拉取；运行中不重建（apply_tool_mode 的允许集过滤见 D4）。
- **C5（后续 phase）skills 替代 schema 描述**：工具使用规则/用例迁入 skills（信封 epoch 增量机制已具备），schema 只留最小骨架（name/参数类型）——防止模型按过时描述误用工具。见决策 D6。

### 3.4 不变式

1. 恢复后 `to_vec()`/`build_context()` 输出 == 崩溃前最后一次请求的消息流（字节级；除 P4 状态变更提示）。
2. compact 后活跃上下文**不含**折叠区注入（摘要已覆盖）。
3. resume 不产生任何 system 前缀字节变化（无 remove、无重排、无重编号）。
4. 崩溃窗口 pending 注入**不被重放**（静默 drop）。
5. schema 变更只以"最新处增量注入"呈现；前缀 tools 字节在会话生命周期内不变（除新 session/compact）。

---

## 4. 任务分解与验收

| # | 任务 | 文件 | 验收 |
|---|------|------|------|
| T1 | A2：`from_messages` 保留原 msg_id（replaying 不重编号；next_msg_id 从全量 max+1） | store.rs | 新测试：构造含 mid-stream `[SUBAGENT]`/developer 的消息集 → from_messages → `to_vec()` 与输入逐字节一致；现有 `from_messages*` 测试全绿 |
| T2 | A1：`apply_compact` 折叠区注入过滤（retain 保留区）+ A3 语义注释 | store.rs | 新测试：注入在折叠区 → compact 后不在活跃上下文、摘要含其文本；注入在保留区 → 位置不变；`compact=Preserved` 相关旧测试更新 |
| T3 | B1：退役 journal（删 replay_pending_injections/mark/API/调用点/测试） | loop_core.rs、manager.rs、engine_input.rs、daemon/ringing_http.rs | 编译通过；rg 确认 `injections.jsonl`/`append_injection_*` 无残留；崩溃恢复测试改为断言"不重放" |
| T4 | B2+B3：删 lifecycle.rs L113-118 前缀移除；meta `clean_shutdown` 标记 + 恢复 user 提示 | lifecycle.rs、types/session.rs、daemon stop 路径 | 新测试：resume 后 system_messages 与崩溃前一致；崩溃 resume 时末尾出现状态变更 user 提示；正常 stop 不出现 |
| T5 | C1+C2：schema 快照落盘 + 指纹校验 + 按类别增量注入 | agent.rs、session 持久化层 | 新测试：同指纹复用快照（tools 字节不变）；异指纹 → 前缀不变 + 最新处出现增量注入（新增/变更/移除三类）；版本升级模拟 |
| T6 | C3：PrefixShape 落盘 + 恢复后首次诊断 | agent.rs | 新测试：恢复后首轮 build_context 若前缀被外部破坏（模拟）能产出 CacheDiagnostics；无破坏则无诊断 |
| T7 | 回归：`cargo test --workspace` + 手工 e2e（模拟崩溃/compact/版本升级，观察 usage `prompt_cache_hit_tokens`） | — | 全绿 + 手工脚本（python 直调 store 模拟崩溃窗口） |
| T8 | 文档：本 PLAN 落为实施记录（语义表 + 已知边界，仿 PLAN-EDIT-FILE-V2 §17） | docs/nextdev/PLAN-FIX-INJECTION-CACHE.md | 文档与代码一致 |

**依赖**：T1/T2 并行 → T3/T4（依赖 store 语义）→ T5/T6（依赖 T4 稳定前缀）→ T7 → T8。T3 可提前（独立退役）。

**风险**：
- T1 改 id 语义影响 `archive_next_id`/turn 计数/timeline 依赖（lifecycle.rs L54-100 有防碰撞逻辑）——T1 回归重点。
- T2 折叠丢弃依赖摘要覆盖：compact 输入含 trailing（已确认 `engine_compact.rs` L101），测试须覆盖"注入文本出现在摘要请求中"。
- T4 删前缀移除的遗留风险：**旧格式会话**（V1 时代残留的过期 skill 信封）可能"复活"——仅影响未升级的旧数据；V2 起由 epoch 机制保证。可选缓解：仅对 `from_resume && meta.schema_version < V2` 的会话执行一次清理（一次性迁移），不做常规路径。
- 环境事故（serve 工具超时未解）：实施期用 bash/python 直调路径（已有先例），不阻塞。

---

## 5. 静默 drop 的逻辑（P3 论证，用户要求解释）

**断言：注入是"一次性消费的运行时事件"，不是 timeline 的一部分；恢复时对未消费注入静默丢弃，不损失任何已发生的事实。**

1. **崩溃前已注入 = 已消费**：注入一旦进入 store（`push_trailing_system` 落盘），它就在后续请求中出现并被模型看到；它存在于 messages.jsonl，恢复时由 from_messages 按原位置回放（T1 后字节一致）。这部分**不属于 drop 范围**——它按顺序成为历史。
2. **崩溃窗口未落盘 = 从未发生**：journal 重放覆盖的窗口是"daemon 已 ACK、worker 尚未写入 store"。该窗口的注入**从未进入任何请求/上下文，模型从未看到**——丢弃它不违反任何"已发生事实"；唯一损失是"一条本来要送达的消息"，而该消息的发起方（daemon 侧 subagent 机制）本就处于崩溃后的不确定状态。
3. **重放的代价 > 收益**：重放会 (a) 在 committed 标记丢失时**重复注入已消费报告**（幂等失效场景）；(b) 使恢复后上下文 ≠ 崩溃前 → **provider 前缀缓存归零**；(c) 让模型把旧报告当作新鲜输入重读（本会话实证的"误放"现象）。而收益仅为"崩溃窗口内最多几条消息不丢"。
4. **事实缺失的补偿 = 状态变更提示**（P4）：若产品要求可见性，恢复时插入 user 提示"未完成的注入已丢弃（N 条）"——以**新消息**告知，而非重放旧消息。这同时满足"上下文如一"与"用户知情"。

**结论：删除重放机制；恢复 = 纯回放 + （可选）状态变更提示。**（决策 D2 需用户确认是否要"数量提示"。）

---

## 6. 缓存前缀构成模型（供 T5/T6 实现参考）

```
provider 请求前缀（字节级稳定 → 自动缓存命中）
├── system_messages[]          （base prompt + env；resume 不得改动 → B2）
├── tools 字段                  （tool_defs JSON；会话内冻结 → C1/C2/C4）
└── 消息流[0..k]               （turn/step/tool_result + trailing 注入按 msg_id 写入序合并）
    ├── 追加（新 turn/新注入）     → 前缀缓存安全（append-only）
    ├── [Environment] 注注入首个 user 消息（frozen，agent.rs L125-132）→ 安全
    ├── 恢复重编号                → T1 后不再发生（字节不变）
    ├── resume 删系统消息         → B2 后不再发生
    └── compact 重建              → 允许破坏缓存（P5 唯二操作之一）
```

---

## 7. 待用户拍板（D1-D6）

| # | 问题 | 拍板 | 状态 |
|---|------|------|------|
| D1 | injections.jsonl journal | **整体退役（B1）** | ✅ 已实施（2026-08-15） |
| D2 | 崩溃窗口注入 | **纯静默 drop**（不提示数量） | ✅ 已实施（退役重放 = 崩溃窗口自然 drop） |
| D3 | compact 折叠区注入 | **直接丢弃（A1，摘要已覆盖）** | ✅ 已实施 |
| D4 | tool_mode/allowed 运行中变更 | 仅新 session / compact 生效（P5 收紧） | 🔲 待拍板 |
| D5 | PrefixShape 落盘粒度 | 全量（hash 数组，小） | 🔲 待拍板 |
| D6 | skills 替代 schema 描述 | 发版后 phase 2（C5） | 🔲 待拍板 |

> **额外拍板（注入时机，2026-08-15）**：① 全部注入统一 user 角色（+name=subagent）；
> ② 注入只能在工具回合完成后的 lap 边界/空闲时插入，绝不夹在 tool_call 与其 tool_result 之间；
> ③ 注入一旦落盘即成为 history（不随错误恢复/重启重放）；④ 注入按当时轮次位置落位，不一直挂最新。
> A2（保留 msg_id）经实证判断**非必需**——`flat_in_write_order` 的写入序合并已保证注入
> 在 `to_vec → from_messages → to_vec` 往返后位置不变（`write_order_merge_survives_full_snapshot_roundtrip`）。

---

## 8. 实施顺序（拍板后）

```
T3（独立退役）→ T1/T2（store 语义）→ T4（resume）→ T5/T6（缓存）→ T7 回归 → T8 文档
```
预计：T1-T4 一轮（store/lifecycle 语义集中），T5/T6 一轮（agent/session 持久化），T7/T8 收尾。
