# 已冻结决策

> 日期：2026-09-25
> 基线：`2.0.0-alpha2`
> 状态：accepted

以下决策是当前实现与后续 debug 的前提；不得在没有新裁决文档的情况下回退。

## D1. 纯 v2，不做 v1 兼容

- `/ringing/v1/*` 路由已删除。
- v1 三频道全局流已删除。
- `Last-Event-ID → v2 cursor` 映射已作废。
- 仍存在的部分 v1 类型/命名只服务内部历史兼容或测试，不得重新暴露为 wire 契约。

## D2. Canonical facts 是事实源

- session 事实以 `events.jsonl` + commit marker 为准。
- timeline、conversation、control、resource 等 projection 都是派生数据。
- 不允许新增第二份可写事实源来绕过 canonical log。

## D3. messages.jsonl 是消息归档

- 消息归档 append-only。
- 工具结果、assistant、user、trailing 注入都按写入序持久化。
- 读取端按 `msg_id` 重建顺序，不按 role 重排协议关键消息。

## D4. compact 由归档水位推导

- 摘要作为普通消息 append 到 `messages.jsonl`。
- `meta.compact_covered_through_msg_id` 是覆盖水位。
- 活跃视图 = system + 最新摘要 + 水位后的非摘要消息。
- `compact-context.json` 和旧 checkpoint effect 不再存在。
- undo / 图片修复的全量重写必须保留或显式清除 compact watermark。

## D5. Ringing v2 单流

- 客户端只订阅单一 v2 流。
- bootstrap 提供 typed 三频道快照。
- reliable replay 与 replaceable current-value mirror 分离：
  - reliable 推进 cursor；
  - replaceable 不带 cursor，只提供当前值。

## D6. Interaction first-answer-wins

- ask / plan / permission 的有效回答只有一个。
- 重复回答返回稳定 `interaction_already_resolved` 和既有结果。
- 不产生第二次工具执行或第二个终态。

## D7. Driver seat 是 canonical 事实

- `DriverChanged` 进入 canonical projection。
- daemon lease 决定 holder 存活。
- 已认领的 seat 对写入命令生效；未认领时保持兼容放行。
- 过期回收、显式 release、daemon 启动轮转均以 canonical 事实收敛。

## D8. 工具终态结构化

- `ToolResult` 状态是终态唯一真相。
- 展示层使用结构化 `outcome`。
- exec 只接受 shell `command` 字符串；直接 `argv` 模式已移除。
- 进程启动层收到的 argv 必须由所选 shell 从 `command` 派生。
- exec stdout/stderr 分离展示，不伪装交织顺序。
- 不从 summary 文本反推执行结果。

## D9. Gate 边界

- `qaqh-gate` 只负责 provider HTTP/协议/流式事件边界。
- agent loop、tool execution、permission、timeline、canonical persistence 不属于 gate。
- 未来接入 `mutilAI-SDK` 必须先经 bridge 和 parity 验收，不能直接替换 runtime 契约。

## D10. 文档权威

- `docs/current/` 是唯一当前权威文档区。
- `docs/archive/` 只用于历史追溯。
- 旧文档中的“未完成/待办/计划”不能直接当作当前状态。

## D11. exec shell 选择

- 配置文件支持 `[exec].default_shell`；空值 / `"auto"` = 平台自动探测。
- 调用级显式 `shell` 参数优先于配置默认值。
- 自动探测顺序：
  - Windows：`pwsh` > Git for Windows bash > `powershell` 5.1 > `cmd`；
  - Linux：`bash` > `zsh` > `sh`；
  - macOS：`bash` > `zsh`。
- 不做跨 shell 命令翻译或兼容降级；不兼容命令由 shell 报错，模型负责修正。

## D12. 工具展示摘要唯一来源

- `display.summary` 只由工具展示投影显式构造，typed 路径不再自动回填模型输出摘要。
- `display.summary` 不承载工具名、终态标记或正文首行；没有更合适的元信息时保持 `None`。
- `TimelineTool.summary` 只在 `display` 缺失时作为 legacy fallback；有 `display` 时不双写。
- 正文只从 `display.body` 或显式 legacy fallback 读取，不再从 `summary` 反推。

## D13. Agent identity 与 delivery 分层

- `AgentId = session_id`，必须全局稳定、可持久化，不随 loaded/unloaded 改变。
- `AgentPath` 是模型可读的 tree address；格式冻结为 `/root[/<segment>]*`，
  segment 只允许 `[a-z0-9_]`，保留 `root`、`.`、`..`。
- 相对 path 只能向当前节点子树解析；跨分支必须使用 absolute path。
- 注入/反注入只负责 delivery，不能替代 AgentPath、graph 或 task 归属。
- 默认只允许同一 root tree 内通信；默认 max depth 为 1，配置可提高到 2。

## D14. AgentGraph 与 canonical facts

- parent-child edge 的权威事实是父 session canonical log 中的
  `SubagentSpawned/Finished`；`AgentGraphStore`、projection 和 SQLite 只做可重建索引。
- 每个 child 最多一个 parent，禁止 cycle；edge 不因 runtime unload 而删除。
- `SessionCreated.parent_session_id` 只作恢复 hint，不能取代 canonical edge。
- graph 读取或重建失败时必须 fail closed，不得猜 topology。

## D15. Mailbox 与 delivery

- 消息接受不等于模型已读；`InterAgentCommunication` 必须显式携带 author、
  recipient、task、trigger 语义。
- V1 只冻结 `queue`、`trigger`、`interrupt`；`steer/interject` 留到第二阶段。
- completion result 默认 queue-only 进入父 mailbox，不得无界触发父 turn。
- wire 上的 `@` 必须是结构化 mention；不得靠正文 regex 推断收件人。

## D16. Status 与 Residency 分离

- `unloaded != completed`；`completed != closed`。
- interrupt 只终止当前 turn，不删除逻辑身份；unloaded agent 仍可被 list。
- delivery 可以触发 reload，但必须经 loaded immediate parent 做 ownership 校验。
- residency 是 daemon-local runtime overlay，不是 durable canonical fact；canonical
  重建默认 unloaded，`loaded` 只在当前进程有 resident worker 时成立。
- V1 `close_agent` 保留兼容；V2 使用 `interrupt_agent` + residency eviction。

## D17. SessionId 是唯一会话主键，seed 必须退场

- `SessionId` 是 canonical UUIDv7，也是 `AgentId`、wire key、runtime key 和目录名。
- 新会话必须满足 `seed == session_id == sessions/{directory_name}`。
- 先生成 canonical identity，再创建目录；不得先建目录再生成另一个 `session_id`。
- `LogId` 继续独立存在，不得与 `SessionId` 合并。
- `seed` 在迁移期只能是 deprecated alias；旧会话的 8 位 seed 只能经 legacy resolver 读取。
- beta 前必须删除新 8 位 seed 生成、canonical producer 中的 seed-as-SessionId、
  wire/runtime 的 seed 语义和旧目录兼容映射。
- 权威迁移设计：
  [`spec/2026-09-25-session-identity-unification.md`](./spec/2026-09-25-session-identity-unification.md)。
