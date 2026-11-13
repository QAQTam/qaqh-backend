# S5 连续性度量报告 — DSX → DeepX-Desktop → deepx(0726) → QAQ-Harness

生成：2026-11-13 · 方法：codegraph 索引 + 逐变体/逐字段 diff + 结构映射 + 示踪符号
对照物：
- **June2** = 本仓重建态 `051d387`（partial，覆盖率见 RECOVERY-MANIFEST.json；agent_protocol.rs 158/158 = 100%）
- **xs** = `C:/Users/QAQTam/Downloads/xs/lib.rs`（74,500 B，deepx-msglp 单体 Loop，仅此一文件，兄弟模块 agent/lifecycle/dashboard/logger 失落）
- **0726** = `D:/Projects/deepx-init` @ `135a5581`（deepx Initial commit，qaqh-backend worktree，完整）
- **May18** = `D:/dsx` @ `aba7c82`（重建提交，源 D:/tools/dsx 孤儿快照）

置信度约定：June2 为会话工具结果重建（非原仓字节），xs 为单文件残片；凡涉及二者的结论均标注「内容连续」而非「提交连续」。0726 与 May18 快照本身是完整文件态。

---

## 1. 协议连续性（S5-2）：agent_protocol.rs 158 行 → 1011 行

文件路径存活：`crates/dsx-proto/src/agent_protocol.rs` → `crates/deepx-proto/src/agent_protocol.rs`（同名文件，crate 前缀 dsx→deepx）。
线格式不变：两端均为 `#[serde(tag = "type")]` + `#[non_exhaustive]` 单行 JSON（JSON-LP）。June2 头注 "channel-based mpsc in-process + JSON-LP headless over stdin/stdout"；0726 proto/lib.rs 头注 "Every frame is a single-line JSON object (\n delimited), tagged with type"——同一设计原话延续。

### 1.1 Ui2Agent：6 → 20 变体

| June2 变体 | wire tag | 0726 归宿 | 字段连续性 |
|---|---|---|---|
| UserInput | user_input | ✅ 同名同 tag | text 原样 |
| ToolCall | tool_call | ✅ 同名同 tag | **id/name/action/args 四字段 100% 原样**（0726 仅加 #[ts(type="any")]） |
| Cancel | cancel | ✅ 同名同 tag | 无字段 |
| Shutdown | shutdown | ✅ 同名同 tag | 无字段 |
| ReloadConfig | reload_config | ✅ 同名同 tag | 无字段 |
| DebugCommand | debug_cmd | ❌ 死亡 | — |

**同名存活 5/6 = 83.3%**；新增 15（CreateSession/UndoTurn/Compact/ResumeSession/NewSession/LoadMoreTurns/SetMode/PermissionResponse/AskResponse/AskDismiss/PlanReview/UnloadSkill/ActivateSkill/SkillOperation/ReloadSkills）。

### 1.2 Agent2Ui：14 → 32 变体

**A. 同名同 tag 存活（6/14 = 42.9%）**：AskUser(ask_user)、Done(done)、Cancelled(cancelled)、Error(error)、ShutdownAck(shutdown_ack)、SessionRestored(session_restored)。

**B. 改名/拆分但字段连续（5/14 = 35.7%）**：

| June2 | 0726 | 字段映射 |
|---|---|---|
| ContentDelta{delta, reasoning} | RoundDelta{turn_id, round_num, kind, delta} | delta 原样；reasoning → kind:RoundDeltaKind::Thinking |
| ApiResponse{content, reasoning_content, tool_calls, stop_reason, usage, context_tokens, context_limit, session_tokens} | **拆分** → RoundComplete{thinking, answer, tool_calls, blocks, is_final} + TurnEnd{stop_reason, usage} | content→answer；reasoning_content→thinking；tool_calls 存活（dsx_types::ToolCall→ToolCallDef，id/name 核心不变，args→args_display+args_json）；stop_reason/usage→TurnEnd；context_limit→Dashboard |
| ToolProgress{id, content, stream_type} | ToolExecDelta{tool_call_id, delta} | id→tool_call_id；content→delta；stream_type 死 |
| ToolResult{id, name, content, success, args} | ToolResults{turn_id, round_num, results:Vec\<ToolResultDef\>} | ToolResultDef{tool_call_id, output, success, file}：id→tool_call_id；content→output；success 原样 |
| DebugSnapshot{hp_connected, session_seed, context_tokens, tool_calls_total, tool_failures, current_phase, streaming, dsml_compat_count} | Dashboard{hp_connected, session_seed, tool_calls_total, tool_failures, current_phase, streaming, dsml_compat_count, context_limit, documents, recent_edits, tasks, …} | **8 字段中 7 个逐字存活 = 87.5%**；context_tokens 死，context_limit 自 ApiResponse 迁入 |

**C. 死亡（3/14 = 21.4%）**：ToolState(tool_state)、CachePrediction(cache_prediction)、Balance(balance)——均为 DeepSeek 时代/KV-cache 调优产物。

**语义存活合计 11/14 = 78.6%**（6 同名 + 5 演化）。纯新增 20（TurnStart/MoreTurns/SessionCreated/ToolNotice/PlanSubmitted/PlanResolved/CompactStart/CompactEnd/CompactDelta/Ready/AuditRecord/ExecProgress/ToolCallPreview/CodeDelta/Pong/SkillsChanged/SkillOperationResolved/PermissionRequest/AskResolved/AskRejected）。

### 1.3 关键字段级示踪

- **SessionRestored**：seed✅ tokens_used✅ cache_hit_pct✅（3/5 逐字）；message_count+summary → turns:Vec\<TurnData\>+total_turns+has_more（结构化升级，非删除）。
- **AskUser**：June2{id, question, options} → 0726 v6{turn_id, round_num, ask_id, mode, questions:Vec\<AskQuestion\>}；AskQuestion{id, question, options, allow_custom}——**question/options 原样下沉**，id→ask_id。三字段全存活，外包 Batch 模式。
- **dsml_compat_count**：June2 DebugSnapshot 字段 → 0726 AgentState 字段 + Dashboard 字段。DSML（DeepSeek Markup Language）兼容计数器跨越整个黑暗期存活，是 DeepSeek 血统的指纹。
- **UsageInfo**：dsx_types::UsageInfo → deepx_types::UsageInfo（类型名不变，仅 crate 改名）。

---

## 2. xs/lib.rs 断代（S5-4）：Round v5 之后、Ring 重构之前的单体 msglp

**定位结论：xs 是 June2 与 0726 之间的失落中间态，填补「Round 协议已生、Ring 引擎未拆」的空窗。**

证据（全部可复验）：
1. **crate 名 = deepx-msglp**（两份 Cargo.toml 均 `name = "deepx-msglp"`）→ 晚于 dsx→deepx crate 改名（June2 之后）。
2. **头注同源**：xs 与 0726 lib.rs 首行逐字相同 "deepx-msglp: message-loop driver for the agent child process."；xs 保留旧版职责清单（1-6 条），0726 改为 Ring 架构图。
3. **已用 Round v5 协议**：`use deepx_proto::{Agent2Ui, Ui2Agent, RoundDeltaKind}`；emit TurnStart/TurnEnd/RoundComplete/RoundDelta/CompactStart/CompactEnd。RoundDeltaKind 在 June2 dsx-proto 中**不存在**（June2 是 ContentDelta）→ xs 晚于 Round 协议迁移。
4. **emit_round_complete 与 0726 同构**：从 deepx_types::ContentBlock{Reasoning,Text,ToolUse} 构建 RoundBlock{Reasoning,Text,Tool{card:ToolCallDef}}，字段 thinking/answer/tool_calls/blocks/is_final 与 0726 RoundComplete 逐一对应。
5. **仍是单体 Loop**：`pub struct Loop` + `impl Loop`，74.5KB 单文件；模块 `pub mod agent; mod lifecycle; mod dashboard; pub mod logger;`。**无 ring/、无 Engine trait、无 RingContext/SessionBundle**（grep 22 命中全为 Ordering::SeqCst / String::new() 的 "ring::" 子串误报，已逐条核实）。
6. **Ui2Agent 分发臂 = 11**，恰为 June2→0726 的中间真子集（见 §3 链）。
7. **早于 0726 的字段级证据**：xs `CompactEnd{summary_chars, turns_compacted}` **缺 turns_removed**（0726 有）；xs 无 AskUser/PermissionRequest/Skill 帧处理（0726 AskUser 标注 v6，需 AskResponse/PermissionResponse 分发臂，xs 的 match 无这些臂）。
8. **依赖差集**：xs 8 个 deepx-* 依赖 = 0726 的 10 个 **减 deepx-skills、deepx-vector**；xs 无 `memory` feature（0726 有）。→ 早于 skills/vector/RAG 引入。
9. **两份 Cargo.toml 差异**：`Cargo (1).toml` 比 `Cargo.toml` 多 `[target.'cfg(windows)'.dependencies] windows = { version = "0.61", features = ["Win32_System_Com"] }`（notify-rust 的 COM 初始化）——同一 crate 的两个相邻修订，Windows 通知修复在后。
10. **取消机制示踪**：xs `deepx_tools::CANCEL.store/load(Ordering::SeqCst)`（AtomicBool）→ 0726 lib.rs 文档 "Propagate Cancel via CancelToken / Arc\<AtomicBool\>"。

**年代窗口**：06-02 之后（crate 已改名）、07-26 之前（Ring 未拆、skills/vector 未入、Ask 未到 v6）。因 Round v5 协议已完整，推测靠近黑暗期后段，但**无精确日期证据**（xs 文件无时间戳锚点，session.json 不覆盖此 crate）。

**模块分解映射（xs → 0726）**：
- `pub mod agent` → `state/agent.rs`（AgentState 存活，见 §4）
- `mod lifecycle` → `state/lifecycle.rs`
- `mod dashboard` → `services/dashboard.rs`
- `pub mod logger` → `util/`（token logging）
- 单体 Loop 的 handle_* 方法 → `ring/loop_core.rs` + 8 个 engine（engine_session/input/compact/misc/turn/tool/goal + engine.rs trait）
- 新增 `services/{conflict,notification,toast_com}`、`state/skill_context`

---

## 3. 单调 superset 链（线性血缘的决定性证据）

Ui2Agent 变体数沿时间轴**只增不减、死变体不复活**：

```
June2 (6)  ──−DebugCommand +CreateSession/ResumeSession/NewSession/LoadMoreTurns/UndoTurn/Compact──▶  xs (11)
xs (11)    ──+SetMode/PermissionResponse/AskResponse/AskDismiss/PlanReview/UnloadSkill/ActivateSkill/SkillOperation/ReloadSkills──▶  0726 (20)
```

- June2 存活 5 ⊂ xs 11 ⊂ 0726 20（严格子集链，已逐臂核对 xs 分发 match）。
- Agent2Ui 同理：June2 的 6 同名 + 5 演化全部在 0726 有归宿，3 死亡变体（ToolState/CachePrediction/Balance）在 xs 与 0726 均不复活。
- 协议版本号自证：0726 agent_protocol.rs 注释 "Agent → UI (v5 — round-based)"、AskUser "(v6)"——June2 为 v5 之前的扁平流式态，xs 为 v5 单体态，0726 为 v5+v6 Ring 态。

**这不是平行相似，而是同一协议族的连续修订。**

---

## 4. 结构映射（S5-3）：crate 与示踪符号

### 4.1 crate 清单演化

| 时代 | crate 数 | 清单 |
|---|---|---|
| May18 | 9 dsx-* | dsx, dsx-agent, dsx-gateway, dsx-hp, dsx-proto, dsx-sudo, dsx-tauri, dsx-tools, dsx-types |
| June2 | 8 dsx-* | dsx, dsx-agent, dsx-hp, dsx-proto, dsx-tauri, dsx-tools, **dsx-tui**, dsx-types（−gateway −sudo +tui） |
| 0726 | 17 deepx-* + 3 apps | deepx-client/companion/config/daemon/gate/gate-testui/message/msglp/proto/runtime/session/skills/subagent/tools/types/vector + ratatui-markdown；apps: deepx-tui/desktop/installer |

### 4.2 逐 crate 归宿

| June2 (dsx-*) | 0726 归宿 | 证据 |
|---|---|---|
| dsx（伞二进制 DSX_SINGLE_BINARY） | deepx-daemon / deepx-runtime | 伞→daemon 概念延续 |
| dsx-agent/orchestrator/gates.rs | **deepx-gate** | explore_gate/re_read_gate/PathTrust → gate crate |
| dsx-agent/runner/{mod,turn,lifecycle,hp_bridge} | **deepx-msglp** → ring/ | run_agent_loop → Loop → ring::loop_core::Loop |
| dsx-agent/assembly.rs (ContextAssembler,Turn) | **deepx-message** (MessageStore) | xs agent.msg.turn_count() → 0726 msg:MessageStore |
| dsx-agent/session/{mod,persist,restore} | **deepx-session** (SessionMeta) | seed/generate_seed 概念存活 |
| dsx-agent/config.rs | **deepx-config** | Config 存活 |
| dsx-agent/agent.rs (AgentState,FileSnapshot) | **deepx-msglp/state/agent.rs** | 见 §4.3 |
| dsx-agent/tool_parser.rs | deepx-tools | parse_dsml_tool_calls → dsml_compat_count 示踪 |
| dsx-agent/health.rs (DsAgentsHealthPlatform) | services/dashboard | hp_connected/current_phase 入 Dashboard |
| dsx-proto | **deepx-proto** | agent_protocol.rs 同名存活；+companion +control；tools.rs/hp.rs 模块消亡（设计并入 lib.rs 文档与 deepx-tools） |
| dsx-types | **deepx-types** | UsageInfo/ToolCall/Message/ContentBlock 存活 |
| dsx-tools | **deepx-tools** | CANCEL AtomicBool 示踪 |
| dsx-tauri (React/TSX) | **apps/desktop (Electron)** | 换壳；前端 types.ts 由 #[ts(export)] 生成（0726 协议全量 TS derive） |
| dsx-tui | **apps/deepx-tui** + ratatui-markdown | TUI 线存活 |
| dsx-hp（May18 已切除监控层，剩 liveness/registry/runner） | 整 crate 消亡 | HP TCP 概念仅存于 0726 proto/lib.rs 文档注释 "Agent → HP \| TCP localhost"（退化痕迹，模块已无） |
| dsx-gateway / dsx-sudo | June2 前已死 | May18→June2 清单差集 |

0726 纯新增：deepx-client/companion/gate-testui/skills/subagent/vector、ratatui-markdown、apps/installer。

### 4.3 跨三时代示踪符号（June2 → xs → 0726）

| 符号 | June2 | xs | 0726 |
|---|---|---|---|
| **AgentState** | dsx-agent/agent.rs `pub struct AgentState`（310 行，"shared between TUI and agent loop"） | `agent: &AgentState`、`agent.msg.turn_count()` | state/agent.rs `pub struct AgentState{msg:MessageStore, config, session, tool_defs, dsml_compat_count, turn_count, ephemeral, vector, skills}` |
| **dsml_compat_count** | DebugSnapshot 字段 | （在失落的 agent.rs） | AgentState 字段 + Dashboard 字段 |
| **msg / MessageStore** | assembly.rs ContextAssembler | agent.msg.turn_count() | msg:deepx_message::MessageStore |
| **seed** | SessionRestored.seed、session::generate_seed | handle_resume_session(seed) | SessionRestored.seed、SessionCreated.seed、SessionMeta |
| **CANCEL** | （tools 内） | deepx_tools::CANCEL AtomicBool | CancelToken / Arc\<AtomicBool\>（文档） |
| **Ui2Agent/Agent2Ui + tag="type" + JSON-LP** | agent_protocol.rs 头注 | use deepx_proto::{Agent2Ui,Ui2Agent} | proto/lib.rs 头注（同设计原话） |
| **agent_protocol.rs 文件名** | 158 行 | （引用其类型） | 1011 行 |
| **UsageInfo** | dsx_types::UsageInfo | deepx_types（依赖） | deepx_types::UsageInfo |

AgentState 是核心会话状态结构，三代同名同职责；0726 仅增 ephemeral/vector/skills 新字段，未改其「TUI 与 agent loop 共享」的本体地位。

---

## 5. 量化汇总

| 度量 | 数值 | 置信度 |
|---|---|---|
| Ui2Agent 同名存活（June2→0726） | 5/6 = 83.3% | 高（两端完整文件） |
| Agent2Ui 语义存活（同名+演化） | 11/14 = 78.6% | 高 |
| Agent2Ui 死亡率 | 3/14 = 21.4% | 高 |
| DebugSnapshot→Dashboard 字段逐字存活 | 7/8 = 87.5% | 高 |
| ToolCall 字段存活 | 4/4 = 100% | 高 |
| SessionRestored 字段逐字存活 | 3/5（余 2 结构化升级） | 高 |
| xs Ui2Agent 分发臂 ⊂ 0726 | 11/11 = 100% 子集 | 高（xs 单文件完整 match） |
| June2 协议重建覆盖率 | agent_protocol.rs 158/158 = 100% | 高（RECOVERY-MANIFEST） |
| crate 名延续（dsx-*→deepx-* 同职责） | 8/8 June2 crate 有归宿（hp 消亡但概念退化存留） | 中高 |
| 线格式（JSON-LP + type tag） | 三代不变 | 高 |

**结论：June2 ↔ 0726 之间存在高置信度的内容连续性（协议语义存活 78.6%、核心结构 AgentState 三代同名、线格式不变、单调 superset 链），但 71 天内零原始提交存世。xs/lib.rs 是该链条上可验证的中间修订。**

---

## 6. S6-1 分段血缘裁决

| 段 | 区间 | 裁决 | 关键证据 | 置信度 |
|---|---|---|---|---|
| **A** | May14→16→18（D:/dsx） | **真实同仓血缘**（提交为重建态，非原始哈希） | .git gitfile → E:/coding/dsx/.git/worktrees/hopeful-einstein-937e5e（母仓失落但 worktree 链接证明同仓族）；9 crate 一致；内容渐进演化 | 高（同仓）；提交哈希为重建 |
| **B** | May18→June2 | **内容连续，提交全失** | 9→8 crate（−gateway −sudo +tui）；proto 仍单 agent_protocol.rs；AgentState/DSML 示踪；品牌 DSX→DeepX 而 crate 仍 dsx-*；改名窗口 05-18→06-02 | 高（内容）；零原始提交 |
| **C** | June2→0726（黑暗 71 天） | **内容连续，提交全失** | §1-§5 全部：协议 superset 链 6→11→20、语义存活 78.6%、agent_protocol.rs 同名、AgentState/dsml_compat_count/CANCEL 三代示踪、xs 填补中间态、Tauri→Electron 换壳 | 高（内容）；零原始提交；xs 精确日期不可考 |
| **D** | 0726→qaqh | **真实，已接通** | ADR-0013 graft（当年旧 HEAD→新根逐字节验证）；qaqh-backend main 含 0726 根 135a5581 | 高 |

**总裁决：DSX→DeepX→QAQ-Harness 是一条真实的内容血缘链。A/D 两段有 git 物证（A 为重建提交+worktree 链接，D 为已验证 graft）；B/C 两段原始提交失落，但内容连续性由协议/结构/示踪符号三重证据高置信度证实。xs/lib.rs 是 C 段唯一存世的中间修订残片。**

---

## 7. S6-2 归档建议：archive ref，而非 graft

**ADR-0013 标准**：接合点必须验证内容连续——当年旧 HEAD→新根源码**逐字节一致**才接。

**本前传不满足 graft 条件**：
- June2 重建态（051d387）→ 0726 根（135a5581）之间隔着 71 天黑暗期：agent_protocol.rs 158→1011 行、8→17 crate、Tauri→Electron、单体 Loop→Ring 引擎。**无法逐字节验证接合点**。
- 且 June2 本身是 partial 重建（非原仓字节），xs 是单文件残片。把重建态/残片硬接为 0726 的父亲 = **伪造 commit provenance**，违背 ADR-0013 的诚实性原则。
- 硬接还会重写 main 的 1410 个哈希，破坏已推送的发布历史（v3.1.20261113-1/-2 tag）。

**建议方案（断代 archive ref，不改 main 父亲、不重写任何哈希）**：
1. 在 qaqh-backend 增加只读归档引用，用 `git fetch <本地路径>` 取对象，**不 merge、不 rebase、不改 main**：
   - `refs/archive/prehistory-dsx` → D:/dsx `aba7c82`（May18 重建态，含 May14/16 重建链）
   - `refs/archive/prehistory-deepx-desktop` → recon `051d387`（June2 partial 重建）
   - `refs/archive/prehistory-msglp-monolith` → xs 残片归档提交（见下）
2. xs/lib.rs + 两份 Cargo.toml 归位：在 recon 仓建 **orphan 分支** `msglp-monolith`（不污染 June2 快照主线），提交 xs 三文件 + 断代说明，archive ref 指向该提交。
3. 写 **ADR-0020「前传与黑暗 71 天」**：记录本报告全部证据链、四段裁决、失落坐标（E:/coding/dsx 母仓、D:/project/DeepX-Desktop 原仓、GitHub QAQTam/dsx 404）、archive ref 清单与「为何不 graft」的 ADR-0013 论证。
4. 归档后 D:/dsx、recon、xs 残片均成为 qaqh-backend 可追溯的只读前传，**真实联系以证据留痕，而非以伪造父链声称**。

**此方案符合宪法 #9（RingingV2 最高准则，不为声称兼容写拧巴转换层——同理不为声称血缘写伪造 graft）与 ADR-0013 provenance 诚实性。**

---

## 附：失落坐标清单（不可恢复，仅留痕）

- `E:/coding/dsx/.git`（母仓，E 盘已不存在）——worktree hopeful-einstein-937e5e 的宿主
- `D:/project/DeepX-Desktop`（June2 原仓，D:/project 整个目录已不存在）
- GitHub `QAQTam/dsx`（remote origin，已 404）
- xs 的兄弟模块 `agent.rs / lifecycle.rs / dashboard.rs / logger.rs`（仅 lib.rs 存世）
- 黑暗期 06-02→07-26 的全部原始提交（git log/status 当时已 30s 超时，历史丢失病因见 June2 会话记录）