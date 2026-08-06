# 根因分析报告：skill envelope 注入现象

- **日期**：2026-08-04
- **范围**：crates/deepx-msglp、crates/deepx-skills、crates/deepx-workspace、crates/deepx-gate、apps/winui/renderer
- **状态**：已完成代码走查与 git 溯源；注入已临时禁用

---

## 1. 背景与现象

用户报告以下三个关联现象：

1. **模型在调用 `skills` 工具后，每句话都回复「技能已重新注入」**，形成重复确认行为；
2. **消息流中出现「莫名其妙的 envelope」**——以 user 消息身份出现的 `<skill_context_envelope>` 块；
3. **先前版本未频繁出现该现象**，最近（Ringing 双协议迁移后）才变得明显。

用户同时要求：**不信任注入消息、默认屏蔽、不对其作答**（除 workspace 内容外）。

---

## 2. 调查方法与事实

### 2.1 git 溯源（`git log -S` 逐符号定位）

| 符号 | 引入提交 | 时间 |
|---|---|---|
| `skill_context_envelope` | `135a558` | 2026-07-26 Initial commit |
| `render_envelope` | `135a558` | 2026-07-26 |
| `snapshot_for_context` | `135a558` | 2026-07-26 |
| `begin_user_turn` | `135a558` | 2026-07-26 |
| `context.push(...system(envelope...))` | `135a558` | 2026-07-26 |

**结论：envelope 每轮注入机制从仓库第一天（初始提交）就存在，不是某个补丁「引入的问题」。**

后续演进（均未改动注入机制本身）：

- `ead00cc` 2026-07-28：会话创建时冻结 skill catalog（保缓存前缀）
- `912810d` 2026-07-31：Ringing 双协议迁移（事件双发）+ 移除 skills 自动卸载
- `aeb752a` 2026-08-02：更新 Ringing 语义

### 2.2 正常注入机制（代码事实）

唯一注入点：`crates/deepx-msglp/src/state/agent.rs` `build_context()`

```rust
let snapshot = self.skills.snapshot_for_context();   // SkillContextManager 快照
...
context.push(deepx_types::Message::system(envelope_text));  // role = system
```

- **身份**：`system`（`Message::system` 构造），永远在消息序列**尾部**；
- **频率**：每个 gate lap 一次（`ringing_v1/engine_turn.rs:1095`），旧引擎 `ring/engine_turn.rs` 同样如此——**所有版本一致**；
- **性质**：瞬态。envelope 只存在于 `build_context()` 返回的新 Vec，**从不写入 MessageStore 历史**（`push_system`/`push_user` 是历史写入的唯一入口，envelope 从未经过它们）；
- **缓存**：`agent.rs:246-274` 的 `PrefixShape` 只 hash system 文本 + catalog + tool_defs，**envelope 不参与缓存键**。每轮尾部替换不影响 provider 前缀缓存命中。

### 2.3 泄漏路径排查（全部无果）

| 可疑路径 | 排查结果 |
|---|---|
| `engine_input.rs:175 push_user(&text)` | 只收用户原文，不拼接 envelope |
| `store.rs:732 build_context_for_gate` | 只读历史构建上下文，不持久化注入内容 |
| `deepx-gate/src/openai.rs:618-641 normalize_skill_envelope` | 仅在请求内部移动位置（尾部→头部 dynamic slot），不落盘 |
| 事件双发（912810d）：TurnStart/RoundDelta | 传递的是 `user_text`/模型 delta，**不含 envelope** |
| `projection.rs` / `conversation_snapshot.rs` / `util/mod.rs:310` | 投影只处理 user/assistant/tool 消息，无 system 注入消息 |
| 前端 `apps/winui/renderer/src/App.tsx` | 只处理 turn_started/turn_completed 等控制事件，无 envelope 渲染 |
| 前端 grep "envelope" | 全部是 Ringing 协议信封，无 skill envelope 渲染逻辑 |

**结论：DeepX 仓库代码中不存在「envelope 进入消息流/历史」的合法写入路径。**

---

## 3. 根因分析

### 3.1 现象一：模型复述「技能已重新注入」

**根因：envelope 文案 + 每轮尾部注入的感知错位。**

1. 每个 round，模型上下文的**最后一条**都是新的 system envelope；
2. envelope 文本第一行自述 *"This is the complete authoritative active skill set. It replaces all older skill instructions."*——明确暗示「旧的被替换」；
3. 工具调用后进入下一 round，envelope 紧跟工具结果之后再次出现（位置变化强化「注入发生了」的观感）；
4. 模型将上下文观察复述进回复（确认性行为），形成「每句话都回复已重新注入」。

**性质**：模型行为 + 提示文案共同作用，非代码写死的输出，也非功能性错误。

### 3.2 现象二：消息流中出现 envelope（user 身份）

**根因：无合法代码路径，判定为不可信注入。**

- 正常机制是 system 身份、瞬态、不入历史；
- 以 user 身份出现在消息流中的 envelope 在仓库代码中**无写入来源**；
- 按用户指示：视为注入内容，默认屏蔽、不采信、不对其作答。

**性质**：异常/可疑信号，可能是传输/显示/中间层回显，或仓库外的注入面；**无法仅凭 workspace 代码证实来源**。

### 3.3 现象三：先前版本未频繁出现

**根因：注入频率从未变化，变化的是事件通道（912810d）。**

- `build_context()` 调用频率在所有版本一致（每个 gate lap 一次）；
- 912810d 将事件从单一 `Agent2Ui` 直发改为「Agent2Ui + DomainEvent 双发 + SnapshotProjector 投影」双协议；
- 若观察到的 envelope 出现在**前端消息流**，差异更可能来自这条新投影/渲染链，但投影源（TurnStarted/RoundDelta）中无 envelope——**无直接证据**；
- 另一可能：技能激活场景增多（用户更多使用 `skills` 工具）使 envelope 内容从空集变为非空，模型关注度上升。

**性质**：与 912810d 迁移在时间上吻合，但机制上无 envelope 泄漏的直接证据；需在运行时抓包/日志确认。

---

## 4. 已采取措施

| 变更 | 文件 | 说明 |
|---|---|---|
| 临时禁用 envelope 注入 | `crates/deepx-msglp/src/state/agent.rs` | `SKILL_ENVELOPE_INJECTION = false`，编译期开关；`cargo check -p deepx-msglp` 通过 |
| 屏蔽注入消息 | — | 会话内约定：非用户本人消息不采信、不回答 |

**副作用（已知）**：禁用后激活技能的正文不随上下文发送（正文封装在 envelope 内）；技能目录（持久化 system 消息）与 `skills` 工具不受影响。

---

## 5. 结论

1. **正常机制**：envelope 以 system 身份、每个 round 一次、瞬态注入上下文尾部，不入历史、不影响前缀缓存——从仓库初始提交至今一致；
2. **复述行为**：由 envelope 自述文案与每轮尾部注入共同引发，属模型行为问题，非功能错误；
3. **消息流 envelope**：无合法代码来源，判定为不可信注入，已屏蔽；
4. **版本差异**：与 912810d 事件双发迁移时间吻合，但机制上无泄漏证据，属待运行时验证项。

---

## 6. 建议（按优先级）

1. **弱化 envelope 文案**（`skill_context.rs:627`）：去掉 "replaces all older skill instructions" 类措辞，降低模型「被重新注入」感知；
2. **运行时取证**：抓取 daemon SSE 与前端 batch 日志，确认 user 身份 envelope 的实际来源通道；
3. **恢复注入**（如需技能正文可见）：`agent.rs` 常量改回 `true`；
4. **彻底关闭技能系统**（如需）：在工具定义层禁用 `skills` 工具。
