# P6 设计输入 A 复核 Handoff（2026-09-24）

状态：**A 按原设计不成立**，已用等价护栏替代。承接
`docs/plan/2026-09-24-p6-上下文结构解耦设计输入-plan.md` §2（issue #339）。

## 1. 复核结论：A 的前提是错的

设计稿 §2.1 断言：

> 会话跑到第 10 轮激活 skill → 新 system 消息拿到高位 msg_id，却插到最前 → 前缀整体失效。

**实测不成立。** 生产路径**只有会话建立的三个入口**会调用
`MessageStore::push_system`：

| 调用点 | 场景 |
|---|---|
| `crates/qaqh-runtime/src/agent/state/lifecycle.rs:385` | `create_session` |
| `crates/qaqh-runtime/src/agent/state/lifecycle.rs:415` | `create_session_with_preset_seed` |
| `crates/qaqh-runtime/src/agent/state/lifecycle.rs:455` | resume 建立 |

其余全部是 `#[cfg(test)]`。运行期注入一律走 `push_trailing_system`：

| 注入 | 路径 | 落位 |
|---|---|---|
| skills envelope | `ContextFlow(SKILLS)` → `Sink::Trailing` | `trailing_messages`（写入序） |
| MCP 清单 | `ContextFlow(MCP_RESOURCES)` → `Sink::Trailing` | 同上（现已默认关闭） |
| subagent 报告 | `ContextFlow(SUBAGENT)` → `Sink::Trailing` | 同上 |
| goal prompt | `ContextFlow(GOAL)` → `Sink::Turn` | `turns`（写入序） |

`push_system_input`（`Sink::Turn` + System/Developer）也是 `turns.push(...)`，**不碰
`system_messages`**。

所以 `system_messages` 生产上恒为 `[base prompt]` 一条，位置稳定；
「中途 push_system 顶到最前」是**潜在**编程错误，不是现存 bug。

## 2. 因此不做 Segment 重构

设计稿 §2.3 提出的 `Segment { Base, Skills, Environment, History }` 分区，是为了让
「会变的段」不整体击穿前缀。但既然：

- `Base` 恒为一条且在建立时写入；
- skills / MCP / subagent / goal 已经全部在**尾部**按写入序追加（缓存友好的位置）；
- `Environment` 注解已由 `frozen_annotation` 冻结并持久化；

那么分区只是**给已经正确的结构加一层标签**，没有可验证的收益，却要动
`system_messages` 的排序、`from_messages` 的恢复规则与十余处测试。**不做。**

## 3. 替代品：把潜在错误变成会红的断言

`MessageStore::push_system` 增加 `debug_assert!`：

```rust
debug_assert!(
    self.turns.is_empty() && self.trailing_messages.is_empty(),
    "push_system 只能用于会话建立（历史为空）；运行期注入请用 push_trailing_system，\
     否则新内容会落到上下文最前面并击穿前缀缓存"
);
```

并补齐方法文档（说明「`system_messages` 渲染时永远前置，与 msg_id 无关」）。

效果：未来任何引入「运行期 push_system」的改动会在**测试构建里立刻失败**，而不是
静默击穿前缀缓存（那是纯性能回归，没有测试会红）。

新增用例：

```text
qaqh-message  push_system_is_creation_only_and_injection_stays_trailing
qaqh-message  push_system_after_history_is_a_programming_error  (#[should_panic])
```

**全量回归验证**：`cargo test --workspace -- --test-threads=1` 138 suites 全绿，
说明**没有任何现存测试**违反该不变量——反向确认了 §1 的结论。

## 4. 验证证据

```text
cargo test --workspace -- --test-threads=1              PASS（138 suites，0 failed）
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo fmt --all -- --check                               PASS
```

## 5. 设计稿需要修正的地方

`docs/plan/2026-09-24-p6-上下文结构解耦设计输入-plan.md` §2 需标注为**已复核 /
不成立**（本 handoff 是权威结论）。§5 的「A + B 同批」随之调整为 **只做 B**。

## 6. 下一步：B 的两种形态（需要裁决）

B 的前提**成立**（已复核）：

- `compact-context.json` 是第二真源：`save_compact_context` / `update_compact_context`
  把整个活跃窗口 `messages.to_vec()` 又写一份，且每次 append 后重写。
- `build_context_for_gate` 每轮 `system_messages.clone()` + `flat_in_write_order()`
  整段 clone → 请求峰值 2×。

但设计稿 §3.3 写的 `ContextCompacted` 作为 **canonical fact** 有一个前提没成立：
`messages.jsonl`（消息归档）与 `events.jsonl`（canonical fact log）目前是**两套并行
存储**，消息正文不在 canonical log 里。所以「把 compact_id 写进 events.jsonl」需要
先统一两套日志——那是更大的一块 P6 工作。

两条可落地路线：

### 路线 1（推荐）：marker 进 `messages.jsonl`

- 压缩时把合成摘要 `[Compacted N turns]` 作为**一条真实消息 append** 到
  `messages.jsonl`（当前它只进 checkpoint，归档里没有）；
- `meta` 记 `compact_covered_through_msg_id`；
- 活跃视图 = `摘要 + msg_id > covered` 的消息，**由归档推导**；
- 删除 `compact-context.json` 的读写与 `PersistOp::{Update,Save}CompactContext`。

代价：动 `apply_compact` / `flush_meta` / `load_for_resume` / `has_compact_context`
的整写分支 / undo 映射；`messages.jsonl` 的行仍是 `Message`，**格式不变**。

### 路线 2：先统一两套日志

把消息正文并入 canonical log，再让 `ContextCompacted` 成为 canonical fact。
收益是 P6「canonical log 单源」的终局；代价是迁移面覆盖所有会话目录 + 双写对账 +
rollback，属于 P6 主线而不是一次增量。

**建议**：先做路线 1（拿掉第二真源 + 峰值 2× 的一半），把路线 2 留给 P6 的
「删除旧目录 / canonical 单源」窗口。

> **落地更新（2026-09-25）**：路线 1 已完成，见
> `docs/handoff/2026-09-25-p6-compact-archive-single-source-handoff.md`。
> 路线 2 仍留给 P6 单源窗口。
