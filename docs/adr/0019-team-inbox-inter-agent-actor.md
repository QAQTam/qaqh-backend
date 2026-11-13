# ADR-0019 Team inbox 跨根署名：InterAgentActor 寻址统一

状态: `已采纳 · 已实施`（读侧 + 决策 4 死变体退役均于 2026-11-13 随 LTS-v3.1 切线落地） · 决策日期 2026-11-13 · 决策人 用户（"直接设计,现在就是 v3.1 窗口"）

契约分类: **[contract-refreeze]**（wire 实变：`TeamInboxSummary.author/recipient`
字段类型 `AgentPath → InterAgentActor`，新增导出类型）。只进 v3.1 线，
与 genui 线既有实变（`ConversationSendMessage.widget_context`、
`ringing-commands-9`、writeScopes）同批；v3.0 金标准不动，切线时机器重切
（宪法 16）。

## 背景

1. ADR-0014 决策 5：跨 root 投递的 `InterAgentEnvelope.author/recipient`
   以 **session id 署名**（path 的 namespace 是 root 相对的，跨 root 歧义），
   同 root 保持 path 署名；判别公理已定——"UUID 含 `-`、绝对 path 以 `/`
   开头，id/路径寻址无歧义"。
2. Team 投递监视（ADR-0014 落地后增补，2026-11-13）把 `unread_messages`
   从硬编码空数组接通到 canonical blocks；但冻结的
   `TeamInboxSummary.author/recipient: AgentPath` 表达不了 id 署名，
   当时只能"只收同根 + 解析失败静默丢弃"——后者正是宪法第 5 条禁止的
   把错误咽掉的补丁。
3. 第二权威清点：`FactPayload::InterAgentCommunication`（fact 面
   `author/recipient: AgentPath` 的 ledger 变体）**没有任何生产构造方**
   ——ADR-0014 决策 3 的"ledger 持久"实施落在 blocks Input 信封上，
   fact 变体成了悬置空壳。

## 决策

1. 新类型 `InterAgentActor`（`qaqh-ringing-contract::fact`，ts 导出）：
   ```jsonc
   {"kind":"path","data":"/root/explore"}      // 同根，绝对 AgentPath
   {"kind":"session","data":"0199a0f0-…"}      // 跨根，session id
   ```
   `from_signature(&str) -> Option<Self>` 就是 ADR-0014 判别公理的类型的化
   （`/` 开头→path 校验；含 `-`→session id；其余=None）。这不是防御性
   兜底：署名由内核构造点产生（host_impl 的 cross_root 分支），None 只
   能来自数据损坏，消费端必须报错而非跳过。
2. `TeamInboxSummary.author/recipient: InterAgentActor`。跨根投递进
   Team inbox；前端拿到的是可判别 union，无需字符串猜测。
3. `InterAgentEnvelope`（wire 命令 + blocks 持久格式）**不改**：string
   署名是它作为"命令元数据/持久格式"的既有事实；寻址类型化只发生在
   投影读模型面。改 envelope 会同时动命令面 + 已落库 payload 格式，
   收益为零、破坏面翻倍——不做。
4. `FactPayload::InterAgentCommunication` 死变体退役登记：删除 FactPayload
   变体 + `InterAgentCommunication` 类型（validation.rs / slots.rs 随行），
   与本轮读侧改动同为 v3.1 refreeze 内容。本轮不实施——避免牵连 fact
   fixture 与 N-1 配对（ADR-0005）扩散；切线清单执行时一并机器重切。
   > **已实施（2026-11-13，切线执行）**：变体 + 类型 + validation/slots 臂已删；
   > 连带孤儿 `InterAgentContent` 类型与 `MAILBOX_TEAM` 常量一并退役（全仓无其他引用，
   > 实测 rg 清零）；fact fixture 无牵连（serde 名 `inter_agent_communication` 全仓零命中）。
   > 导出物差额由 v3.1 金标准机器重切吸收（ADR-0010 §切线实施记录）。

## 被否选项

- **保留 AgentPath + 附加 `author_session_id: Option`**：跨根时 path 字段
  要么空串要么伪造，两个字段表达一个事实=第三权威（宪法 9）。
- **信封同步改类型**：破坏已落库 payload 格式，逼出迁移层（宪法 4）；
  持久格式与读模型形状本就允许不同代际。
- **只修投影、类型不动（维持"只收同根"）**：跨根投递永久在 Team 面
  不可见，用户正在用的"找并行会话咨询"场景恰恰是跨根——能力悬置。

## 后果

- v3.1 实变清单 +2：`InterAgentActor` 新增、`TeamInboxSummary` 字段类型变。
  **已记 ADR-0010 §攒着的 refreeze 清单**（切线时一次吸收，机器重切 golden）。
- `qaqh-client` 的 `/team` 形状测试 fixture 同步改（同一轮提交）。
- ADR-0014 增补段"跨根条目入 inbox 属 v3.1 议题"由本 ADR 兑现。
