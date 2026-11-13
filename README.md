# deepx-msglp monolith intermediate (xs/lib.rs) — 断代残片

来源：`C:/Users/QAQTam/Downloads/xs/`（session.json 同期出土文物）。
本 orphan 分支归位黑暗期（June2→0726）唯一存世的中间修订残片，**不污染 June2 快照主线**。

## 文件

- `lib.rs`（74,500 B）：deepx-msglp 单体 Loop。模块 `pub mod agent; mod lifecycle;
  mod dashboard; pub mod logger;`。兄弟模块 agent.rs/lifecycle.rs/dashboard.rs/logger.rs
  **失落**，仅 lib.rs 存世。
- `Cargo.toml`：deepx-msglp，8 个 deepx-* 依赖（gate/config/session/message/tools/
  subagent/proto/types）。
- `Cargo.windows-com.toml`（原 `Cargo (1).toml`）：比 Cargo.toml 多
  `[target.'cfg(windows)'.dependencies] windows = { version = "0.61",
  features = ["Win32_System_Com"] }`（notify-rust COM 初始化）——同一 crate 相邻修订，
  Windows 通知修复在后。

## 断代结论（S5 报告 §2，详见 main 史 docs/history/prehistory-continuity-S5.md）

**Round v5 协议迁移之后、Ring 引擎重构之前**（06-02～07-26 之间，精确日不可考）：

1. crate 名 = deepx-msglp → 晚于 dsx→deepx 改名（June2 之后）。
2. 头注与 0726 lib.rs 首行逐字同源 "deepx-msglp: message-loop driver for the agent
   child process."。
3. 已用 Round v5：`use deepx_proto::{Agent2Ui, Ui2Agent, RoundDeltaKind}`；emit
   TurnStart/TurnEnd/RoundComplete/RoundDelta/CompactStart/CompactEnd。RoundDeltaKind
   在 June2 dsx-proto 不存在 → 晚于 Round 迁移。
4. `emit_round_complete` 从 deepx_types::ContentBlock{Reasoning,Text,ToolUse} 构建
   RoundBlock，与 0726 RoundComplete 同构。
5. 仍单体 Loop（pub struct Loop + impl Loop），**无 ring/、无 Engine trait、无
   RingContext/SessionBundle**。
6. Ui2Agent 分发臂 = 11，恰为 June2(6)→0726(20) 的中间真子集。
7. 早于 0726：CompactEnd 缺 turns_removed；无 AskUser v6/PermissionRequest/Skill 帧。
8. 依赖比 0726 少 deepx-skills/deepx-vector，无 memory feature → 早于 skills/vector/RAG。
9. 取消机制 deepx_tools::CANCEL AtomicBool → 0726 CancelToken/Arc<AtomicBool>。

## 在血缘链中的位置

```
June2 DeepX-Desktop (dsx-* crate, ContentDelta 扁平协议)
  → [crate 改名 dsx→deepx] → [Round v5 协议迁移]
  → ★ xs/lib.rs 单体 deepx-msglp Loop（本残片，Round v5、pre-Ring）
  → [Ring 引擎拆分 + skills/vector + Ask v6 + permission]
  → 0726 deepx Initial commit (ring/ 架构, 17 crate)
```

本残片是 C 段（黑暗 71 天）内容连续性的关键实物证据：它证明 Round 协议与 Ring 架构
之间存在一个可验证的单体中间态，而非凭空跳跃。