# 第一份架构简化报告：SQLite WAL 落盘与删除清单

- 日期：2026-11-02。
- 审查基线：`d88b90cf` 及当时的未提交工作树，源码现状优先于旧文档和注释。
- 状态：架构分析与删除建议；仅更新报告，没有实施代码改造。
- 最新存储决策：用户已确定采用 SQLite + WAL 作为唯一业务存储，取代初版 JSONL 主存储方案。JSONL 只用于显式导入导出，不双写为第二份权威数据。
- 文件名保留初版名称；正文以本次 SQLite WAL 修订为准。
- 本轮范围：会话持久化、agent 执行组织、内部摘要与恢复机制。
- 协议方向：继续 HTTP 状态快照 + SSE；恢复旧 timeline 同步模型的简单语义，不整笔回滚旧实现。具体字段、身份、回执和重放承诺继续审查。
- 资源记录：磁盘可用约 68 GiB，内存可用约 4.4 GiB，CPU 8 核；本次没有运行编译、全量测试或压测。

## 1. 结论

当前最该删除的不是 `sha2` 这个依赖名称，而是围绕普通会话建立的事务辅助文件、进程内执行凭证、持久化恢复计划和重复校验链。

项目已经有 JSONL，却仍需要 commit marker、摘要链、writer fence、poison marker、recovery intent、replay manifest 和逻辑时钟来解释哪些记录可以使用。列表、团队任务和消息看板还各自管理索引或提交辅助文件，应用已在手写部分数据库职责。

新的目标是：**SQLite 负责业务数据、事务与索引；会话 owner 负责业务顺序；timeline / SessionState 负责前端读模型；HTTP/SSE 负责快照与变化传输。** 不在数据库外重建提交协议，也不为尚未发生的故障预先建立通用恢复系统。错误发生后明确报错，需要修复时针对实际问题处理。

这条路线保留数据库提供的事务能力，主动放弃应用自建的故障对账编排、摘要链完整性证明和自动续跑。SQLite 的正常恢复行为由数据库负责，不能将删除应用恢复框架误写成关闭数据库自身的事务与日志。

SQLite 取代的是自制存储机制，不是把庞大的 canonical 执行框架搬进数据库。只把 `SessionFact` 存进一张表而继续全量重放、重复投影与执行摘要，无法完成这次简化。

## 2. 删除原则

1. SQLite 是唯一业务数据权威，写入经过统一入口，读取直接使用明确的业务查询；不双写 JSONL，也不强制所有查询重放统一事实流。
2. 数据可以直接保存，就不为了内部形式要求先哈希、改名、外置，再读回来。
3. 私有且由所有权传递的请求对象，不再通过密码学摘要证明自身没有改变。
4. 错误直接返回；不自动切换到另一条写入路径，不无差别重启 agent，不补写一套修复文件。
5. 保留实际业务数据、任务句柄和取消能力；删除没有实际消费者的中间状态与证明对象。
6. 删除旧实现时同步迁清调用者、导出、配置和专属测试，不留长期兼容层。
7. 公开协议是否需要某项保证，留到下一轮判断；本轮不因内部机制冗余而擅自删除公开字段。

“可以扔掉”指职责和实现可以退出，不代表直接删除几个文件后仓库就能编译。仍在使用的少量基础函数应移动到剩余模块，而不是为了几个函数保住整套旧框架。

## 3. 源码证据

以下链接与行号对应审查时的工作树。性能影响来自静态调用链分析，没有测得具体耗时、峰值内存或收益比例。

| 编号 | 已确认事实 | 源码依据 |
|---|---|---|
| E01 | 新 batch journal 仍维护 `commit.json`、`storage-format`、writer lock、batch 摘要链及 receipt 索引 | [canonical/batch.rs](../crates/qaqh-session/src/canonical/batch.rs#L22) |
| E02 | 一次 batch 写入先同步 journal，再原子写 marker；失败进入 fenced/unknown 路径 | [canonical/batch.rs](../crates/qaqh-session/src/canonical/batch.rs#L611) |
| E03 | 新 batch 与旧 canonical log、reader、recovery、tool ledger 同时被模块导出 | [canonical/mod.rs](../crates/qaqh-session/src/canonical/mod.rs#L23) |
| E04 | registry 仍检查旧 `events.commit.json`，缺失便返回空事实，再调用旧 reader | [registry/queries.rs](../crates/qaqh-runtime/src/registry/queries.rs#L352) |
| E05 | daemon 启动仍处理旧 canonical writer fence | [server.rs](../crates/qaqh-daemon/src/server.rs#L481) |
| E06 | 私有且不可克隆的执行对象在消费时重新序列化 fact，计算 SHA-256 并比较自身摘要 | [session_commit.rs](../crates/qaqh-session/src/session_commit.rs#L147) |
| E07 | provider 请求身份计算多个摘要，构造阶段和执行验证阶段各调用一次 | [provider_job.rs](../crates/qaqh-agent-runtime/src/provider_job.rs#L88)、[执行验证](../crates/qaqh-agent-runtime/src/provider_job.rs#L499) |
| E08 | blob 读取本身校验摘要，提交层、上下文投影层还会再次校验 | [canonical/blobs.rs](../crates/qaqh-session/src/canonical/blobs.rs#L149)、[session_commit.rs](../crates/qaqh-session/src/session_commit.rs#L348)、[context_projection.rs](../crates/qaqh-session/src/context_projection.rs#L165) |
| E09 | 提交引用验证会把 blob 读回，再调用 `put_durable()`；`put_blob()` 原本已走持久化写入 | [canonical/batch.rs](../crates/qaqh-session/src/canonical/batch.rs#L846)、[session_commit.rs](../crates/qaqh-session/src/session_commit.rs#L368) |
| E10 | 每次新提交先 clone 完整 context 和业务 state | [session_commit.rs](../crates/qaqh-session/src/session_commit.rs#L389) |
| E11 | 读取 metadata 会重建完整 `ProjectionSet`，并从 conversation snapshot 统计回合数 | [meta.rs](../crates/qaqh-session/src/meta.rs#L48) |
| E12 | 命令投递失败没有按原因分流，随后关闭、重建并重试 agent | [registry/messaging.rs](../crates/qaqh-runtime/src/registry/messaging.rs#L184) |
| E13 | host 所谓保留 driver 恢复材料，实际只构造描述字段并记日志，没有把失败 owner/mailbox/input 移交恢复管理者 | [session_agent/host.rs](../crates/qaqh-agent-runtime/src/session_agent/host.rs#L254) |
| E14 | 旧 SessionActor 带有 turn 状态机；registry 的订阅调用却只是同步 submit 后立即 step | [actor.rs](../crates/qaqh-session/src/actor.rs#L479)、[registry/mod.rs](../crates/qaqh-runtime/src/registry/mod.rs#L416) |
| E15 | `canonical_interaction_id` 仍通过 SHA-256 派生 ID；仓库内搜索未发现实际调用者，仅见定义、导出和注释 | [agent/util/format.rs](../crates/qaqh-agent-runtime/src/agent/util/format.rs#L19) |
| E16 | content 逻辑时钟本身另有文件、重建与原子持久化过程 | [canonical/clock.rs](../crates/qaqh-session/src/canonical/clock.rs#L13) |
| E17 | 工具执行已有结构化结果和 effects，但 tool job 的成功返回只保留正文 bytes 和 backgrounded | [execution.rs](../crates/qaqh-workspace/src/execution.rs#L14)、[tool_job.rs](../crates/qaqh-agent-runtime/src/tool_job.rs#L636) |
| E18 | 列表 JSONL 索引需要自行归并 upsert/remove，并按阈值 compact | [store/mod.rs](../crates/qaqh-session/src/store/mod.rs#L55) |
| E19 | TeamStore 与 BoardStore 分别维护身份文件、commit marker 和文件锁 | [team/store.rs](../crates/qaqh-session/src/team/store.rs#L18)、[team/board/store.rs](../crates/qaqh-session/src/team/board/store.rs#L19) |

## 4. 可以整套退出的持久化机制

| 对象 | 涉及实现 | 删除后的最小处理 |
|---|---|---|
| commit marker 与独立高水位 | `canonical/batch.rs` 的 Marker，旧 `EventsCommit` 及修复代码 | 使用 SQLite 事务提交，不在数据库外维护另一份提交真相 |
| batch 哈希链 | `previous_digest`、`digest`、`batch_digest()` | 删除应用层链完整性证明，不叠加在 SQLite WAL 外 |
| writer lease 与 fencing token | 旧 `canonical/log.rs`、`types.rs`，daemon stale fence 轮换 | daemon 单实例、会话 owner 排业务顺序，数据库管理实际写入锁 |
| 每会话 writer 文件锁 | `canonical/batch.rs` 的 `writer.lock` 及旧 events lock | 统一数据库写入入口，删除应用每会话锁文件；SQLite 自身的锁继续工作 |
| poison marker | `events.poison.json` 及写入、解析、恢复状态 | 返回数据库错误，结束失败操作，不另写隔离标记文件 |
| upgrade fence 与格式隔离侧文件 | `upgrade-fence.json`、`storage-format` 及关联状态 | 用最小数据库 schema 版本与明确升级步骤，不保留长期双存储适配 |
| 持久化恢复计划 | `recovery.rs`、`recovery_executor.rs`、`recovery_state.rs` | 重启后识别未完成业务，不自动重放外部工具，不生成恢复事务 |
| replay window manifest | `replay_window.rs` | SSE 使用进程内有界回放尾；无法补齐则取数据库状态快照 |
| 持久化 content clock | `clock.rs` | 随窗口和复杂 GC 机制一起删除，不为取消的功能保留时钟 |
| identity sidecar | `canonical-identity.json` 的独立管理 | 业务身份保存于数据库；协议仍需要哪些身份在协议审查中确定 |
| 旧日志代际 | `CanonicalLog`、`CanonicalSessionStore`、`CommittedFactReader`、`ToolLedger` | 调用者统一接入 SQLite 业务查询与写入，然后删除旧实现 |
| 列表文件索引及 offset index | `index.jsonl` 折叠与 compact、`canonical/offset_index.rs` | 业务表和必要的 SQL 索引，直接查询和分页 |
| Team/Board 自制提交协议 | 各自的 identity、commit marker、events lock 与恢复代码 | 团队任务、看板数据进入同一数据库，通过事务更新 |
| 专属故障注入状态 | `BatchFaultPoint`、marker 故障点及对应字段 | 与被删除的持久化协议一起移除 |

当前 `canonical/batch.rs` 不适合通过删除几个 if 来完成这次简化。它的核心抽象就是“journal + marker + chain + receipt + reconciliation”。由 SQLite 事务和索引接替其存储职责，删除旧框架，而不是在数据库外围保留旧结构。

`log.rs`、`recovery.rs` 内仍有新路径复用的错误类型、原子写辅助和哈希函数。清理时只移动仍有实际消费者的部分；数据库业务访问不应继续依赖“canonical recovery”命名空间。

## 5. 可以退出的进程内证明与包装

| 对象 | 删除判断 | 最小替代 |
|---|---|---|
| `CommittedExecution` 的 fact digest 及 `into_parts()` 自校验 | 删除摘要证明职责 | 一个拥有本次实际请求数据的结构体，按所有权传递 |
| `DispatchTicket` 与二次 authorize | 删除重复许可步骤 | owner 直接提交已经准备好的任务 |
| `EffectIdentity::intent_digest` | 删除 | 完成消息使用任务 ID 关联 |
| commit 的 issued 历史与 supervisor 的多份防重历史 | 删除重复执行证明层 | 正在运行的任务表；外部命令去重另行处理 |
| 每轮 provider/params/tools/prefix 多次摘要 | 删除循环内重复指纹计算 | 传递本次使用的不可变配置与请求数据 |
| wire ID 的内部哈希改形 | 内部改形需求可以删除；公开格式留下一轮审查 | 使用原始 ID 或明确分开的业务 ID，不建立摘要别名链 |
| `CommitGate` | 当前业务调用面未发现接线，删除 | 不保留未落地的提交协调原型 |
| completion 字节预占、resize、redeliver 协议 | 删除这套通道容量证明机制 | 有界通道；大正文保存后，消息只携带引用 |
| `RetainedDriverFailure` 描述包装 | 删除“保留恢复材料”的虚假职责 | 返回明确错误，并实际管理剩余任务 |
| `AgentTransport::ThinAgent` 单变体枚举 | 删除包装 | 直接保存 handle/task |
| 旧 reader/thread 等始终初始化为空的字段 | 删除旧生命周期残骸 | 只维护当前任务模型实际使用的字段 |

新的每个会话 owner 使用独立邮箱。旧任务继续向旧邮箱返回结果，无需仅为防止串入新 owner 而建立复杂的持久化 incarnation 体系。正在运行的任务仍需有句柄与 ID，取消时仍需处理实际外部工作；这些是运行职责，不应被凭证删除掩盖。

`qaqh-agentloop-core` 可以并回 `qaqh-agent-runtime` 的私有模块。保留实际使用的消息接收、任务管理和取消代码；不因“通用循环基础设施”这个名称继续维持独立的授权系统。

旧 `SessionActor/TurnCore` 可以删除。订阅状态直接由 `SubscriberRegistry` 管理，省掉同步调用中 `submit → step → effect` 的包装往返。

## 6. 可以删除的重复劳动与自动兜底

### 6.1 存储与上下文

- 不再在存储、提交、投影三个层次重复计算同一 blob 的摘要。
- 不再将已经写入的内容读回并重新持久化一次。
- 普通输入、模型文本和小型工具结果直接保存在数据库字段内，不强制一律进入内容寻址 blob。
- 不再为每条小记录深拷贝整份上下文与全部历史状态。
- metadata 与列表直接查询业务表，不重建所有会话投影。
- 去掉多份完整客户端投影的无目的常驻副本；模型上下文和客户端视图各保留其确实需要的数据。

模型上下文与 UI 视图具有不同用途，不应为了减少结构体数量硬合并。删除目标是重复持有和重复计算，不是消除业务上真实存在的差异。

### 6.2 错误处理

- 删除 `send_ringing()` 中任意 write 错误触发 close/spawn/retry 的路径。
- 删除运行中 actor 写入失败后再尝试直接打开第二个 writer 的通用 fallback。
- 删除把无法读取、无法解析与不存在统一吞成默认空状态的处理。
- 删除与旧恢复系统绑定的自动修复、自动续跑和自动降级。

邮箱满就返回忙，参数错误就返回参数错误，存储失败就返回存储失败。失败事务在数据库访问边界结束或回滚，不泄漏给后续操作；这里不需要另建 fenced 状态机、持久化隔离文件和自动修复执行器。数据库不可用时不静默回退到 JSONL，不自动重试外部工具。

## 7. 已确定的 SQLite WAL 存储模型

### 7.1 唯一业务存储与职责

使用一个本地逻辑数据库起步：

```text
<data_dir>/qaqh.db
<data_dir>/qaqh.db-wal   # SQLite 管理
<data_dir>/qaqh.db-shm   # SQLite 管理
```

WAL 与共享内存文件可能随连接与 checkpoint 生命周期出现或消失。它们是同一数据库的组成部分，不是应用另建的提交权威；应用不手工删除或截断它们。[SQLite WAL 文档](https://sqlite.org/wal.html)

业务数据的权威在数据库；timeline / SessionState 是当前会话的内存读模型；HTTP 获取状态，SSE 发送变化；JSONL 只做显式导入导出。不另外保留永久 timeline 快照文件、canonical 文件日志或 SSE 事件数据库作为第二份权威。

本轮确定业务存储选型，不顺带变更密钥、证书、第三方工具协议文件的管理方式，也不选择具体 Rust 数据库封装库。

### 7.2 业务表与查询

先按实际业务访问组织数据，不按现有 Rust 结构体数量建表：

| 数据 | 保存内容 | 直接查询的用途 |
|---|---|---|
| 会话 | 身份、标题、工作区、设置、归档与更新时间 | 列表、筛选、排序、改标题 |
| 回合与消息块 | 完整正文、稳定 ID、顺序、角色与真实终态 | 历史分页、恢复会话、构建 transcript |
| 工具调用 | 调用身份、参数、结果与执行状态 | 工具卡片、模型上下文、未完成状态 |
| 交互 | 问题或权限请求、决定、状态 | 待处理列表与应答 |
| 团队任务和消息看板 | 实际任务、领取、依赖、消息与归属 | 任务管理、代理协作与查询 |

表名与字段在实施时按实际调用确定，本表不冻结完整 schema。经常筛选、排序和关联的字段使用列与必要索引；复杂正文、模型特有内容与扩展字段可以继续保存 JSON。完整输入必须保存，不能把展示摘要当成恢复原文。

模型上下文与人类 transcript 可以读取同一批原始业务数据，但构建方式分别服务各自用途。压缩后的模型视图不能替代用户历史。SQLite 的采用不意味着把 canonical 所有内部事实继续永久保存，也不要求每次读会话全量重放事件。

### 7.3 WAL 与写入基线

用户已确定 `journal_mode=WAL`。实施起始建议使用下列连接初始化基线；这部分尚未写入运行代码：

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
```

`WAL` 是确定的选型；`FULL` 是本报告提出的持久化起始设置，不冒充用户额外指定。`NORMAL` 可以作为后续实测后的取舍，它可能在掉电或系统崩溃后丢失最近已提交事务。应用不另外手工 fsync 数据库文件。[SQLite 同步设置](https://sqlite.org/pragma.html#pragma_synchronous)

统一初始化需要在实际写连接落实设置，并读取 `journal_mode` 的结果确认已进入 WAL；不能只执行 SQL 后假定已生效。依赖版本选用包含官方 WAL-reset 修复的 SQLite（3.51.3 及以后，或官方修复分支）。[journal_mode 行为](https://sqlite.org/pragma.html#pragma_journal_mode)、[官方修复说明](https://sqlite.org/wal.html#walreset)

写入组织保持简单：

1. 一个 daemon 统一持有本地数据库，所有写操作进入一个串行写入执行器；会话 owner 继续决定各自的业务顺序。
2. 写事务只包含需要一起生效的数据库修改，模型网络请求、shell 执行和等待用户回答均在事务之外。
3. 数据库工作放在阻塞执行边界，避免同步 SQL 占住异步网络任务；不建设通用数据库服务、复杂连接池或 Repository 框架。
4. 事务提交成功后再更新相应内存视图和发布可持久化业务变化。提交失败则结束本次操作，不伪装成已完成。
5. SQL 唯一约束、条件更新和普通事务用于表达真实数据关系，不另加执行摘要和应用 writer fence。

WAL 可以让读者与写者并行，但同一数据库仍只有一个并发写事务。读查询按需要使用少量连接并尽快结束；SSE 长连接不持有数据库读事务。[SQLite 事务并发说明](https://sqlite.org/lang_transaction.html)

SQLite 不把外部工具副作用与数据库 COMMIT、网络回执变成同一笔原子操作。失败或断线时仍需正确表达结果未知，但不因此复活执行票据和通用对账框架。

### 7.4 checkpoint、流式正文与 HTTP/SSE

先使用 SQLite 默认自动 checkpoint，不新增 checkpoint 调度器、定时 VACUUM、WAL 文件清理线程或独立持久化监护任务。出现实际问题后再按测量结果调整。[自动 checkpoint](https://sqlite.org/wal.html#automatic_checkpoint)

token 进度更新内存视图并通过 SSE 推送，适当合并，不能每个 token 提交一笔事务，也不能反复落盘累计全文造成写放大。输入、最终正文、工具结果和必要业务状态在明确边界保存；异常退出时，尚未保存的进度允许丢失，不能承诺从数据库精确恢复每个中间 token。

HTTP 状态快照从数据库及会话 owner 的当前内存视图组合，SSE 使用同一 owner 串行边界安排快照交接与后续变化。数据库事务不跨 HTTP/SSE 连接存活。数据库提交成功但 SSE 未送达时，持久化数据不回滚，客户端通过快照重新同步。

恢复 10 月 28 日值得保留的同步语义：按 session 订阅、单调流序号、`Last-Event-ID`、进程内有界回放尾，以及无法补齐时 reset 后 HTTP 拉快照。流序号服务于当前 epoch 的变化流，不强制等于数据库行号或内部 fact_seq；daemon 重启重新取快照，不承诺跨进程重放所有中间帧。`retry:` 的传输约定与客户端消费仍属于后续协议整理。

SQLite WAL 是数据库内部提交日志，不是 SSE replay journal；不能将 WAL 页或数据库内部位置导出为客户端游标。

### 7.5 重启、导出与迁移

重启时由 SQLite 完成其正常日志恢复，应用读取业务表恢复会话视图。读取本身不执行工具；未完成工具显示为中断或结果未知，不统一标记成功，也不自动重跑。数据库损坏或不可用时明确报错，不自动切换 JSONL、不编排应用层修复计划。

JSONL 用于显式导入导出与交换数据，不作为后台同步副本。普通正文直接存数据库；二进制可使用 BLOB，不为了维持文本格式额外做 base64 落盘。大附件的具体布局按实际访问确定，本轮不建设内容寻址、跨会话引用计数和 GC 框架。

数据库版本使用最小 schema 版本与明确的向前升级步骤，不引入通用迁移平台。旧存储数据可做一次性导入或按实际需求退役，代码切换后不保留自动双格式读写和运行时 fallback。

若提供运行中数据库备份，使用 SQLite 的备份能力或其他官方支持方式，不把只复制主 `.db` 文件当作完整在线备份。[SQLite Backup API](https://sqlite.org/backup.html)

## 8. 删除后明确放弃的能力

| 进一步简化 | 放弃的能力 | 仍需留下的实际功能 |
|---|---|---|
| 删除存储摘要链与应用全文件校验 | 应用层文件链完整性证明 | SQLite 事务及数据库错误报告，必要的业务校验 |
| 删除应用恢复计划与对账执行器 | 自动外部副作用对账、自动续跑 | SQLite 正常恢复与业务表查询，暴露未完成状态 |
| 删除 quota 的持久化 reserve/commit/release 账本 | 跨 daemon 重启延续预算和占用对账 | 运行期间的计数或并发限制 |
| 审计改为普通操作记录 | 审计哈希链完整性校验 | 有业务用途的记录入库，诊断日志不成为业务权威 |
| 删除跨会话内容去重、引用计数和 GC | 自动共享和自动回收 | 内容可读取，按会话进行粗粒度清理 |
| 删除应用多层同步屏障 | 应用自制文件提交过程 | SQLite WAL 与明确的 synchronous 设置，不削掉数据库正常提交能力 |

quota、审计与内容系统的上述项是能力取舍候选，不能当成已经证明无用的死代码。这些模块未在本轮逐行穷尽审计。实施时针对实际使用路径确认即可，不扩大成新一轮通用架构建设。

同步删除宣称这些能力的注释、配置、诊断指标和专属测试。只验证仍然承诺提供的行为，不维护已删除框架的测试替身。

## 9. 不应混作防御性设计删除的内容

- 完整输入、模型输出、工具效果和身份数据：删了会直接损失真实业务功能。
- 任务句柄、任务关联、取消通知和有界通道：用于正在发生的执行与资源管理。
- 设备授权、路径权限和沙箱：属于已有的访问与执行边界。
- 工具返回的结构化 effects：必须被 owner 消费，不能压成正文后丢弃。
- 网络重试涉及的命令去重：是否跨重启、保留多久以及是否检测载荷冲突，下一轮审查；本轮只否定重复证明层。
- RingingV2 当前公开字段和语义：本报告没有替协议作删除决策。

这份报告不继续细化 QueueOnly、Steer、Interject、系统注入、子代理终态等协议问题。前轮已经观察到它们在内部转换中丢失，本轮只记录这一事实：删除内部框架不能以继续丢失业务语义为代价。

## 10. 建议执行顺序

1. 下一轮审查协议，确定实际要保留的身份、内容引用、回执和重放语义。
2. 建立 SQLite WAL 业务存储与必要 schema，提供直接查询和短事务写入；不建长期双格式适配，也不把所有读取重新接到全量 fact replay。
3. 删除 marker、旧 reader/writer、fence、poison、恢复计划、manifest 和 content clock。
4. 删除执行摘要、重复票据、旧 actor、空字段和独立循环框架包装。
5. 删除重复 blob 操作、全历史 clone、全投影 metadata 读取和无差别重启。
6. 对选择放弃的能力同步清理文档、配置和测试，不追加替代框架。

旧数据使用一次性转换还是明确退役，在实施时根据实际数据需求决定。两种做法都不需要把旧格式永久带进新运行路径。

完成标准看职责是否真正消失：读取业务数据不再依赖应用自建的 marker 和 sidecar，执行任务不再需要摘要凭证，错误不再触发隐式换存储路径，源码与文档不再承诺已经删除的能力。SQLite 自己管理的 WAL/SHM 不属于待删除的应用辅助文件。这里不设预测性的删行数或性能提升指标。

后续实施只验证实际边界：关联写入的事务结果、列表与分页正确性、重启后真实状态，以及快照与 SSE 的交接。删除旧恢复框架的专属测试，不重建一套故障模拟平台；本次文档修订没有运行数据库或业务测试。

## 11. 下一份报告的问题清单

下一轮只围绕协议本身展开，不沿用“现有字段已经存在，所以必须永远保留”的前提：

- 命令、事实、投影是否存在不必要的多套词汇与转换。
- `session_id/log_id/event_id/command_id/input_id/execution_id` 各自解决什么实际问题。
- `ContentRef`、摘要、外置内容和大小阈值是否确有必要。
- accepted、started、finished 与客户端重试各自承诺什么。
- revision、cursor、replaceable、reset 和重放窗口是否存在重复机制。
- 普通用户输入、代理间消息与工具结果能否用更少的协议实体完整表达。

已确认的方向是 SQLite + WAL 唯一业务存储、HTTP 状态快照 + SSE、timeline 作为读模型、canonical 收缩职责。后续报告继续确定协议细节；Rust 数据库库选型、完整 schema、数据迁移和代码实施均尚未完成。

文档核验：Context7 在前轮无沙箱请求中两次返回 `fetch failed`；本次沿用并补查 SQLite 官方 WAL、事务、PRAGMA 与备份文档，没有将网络失败描述为已获取 Context7 文档。

## 12. 最终复核追加：八类架构删减与缺口补齐

本节按 2026-11-02 的后续讨论追加。复核范围为 `docs/` 当前仍存在的两份 Markdown；工作区中此前已删除的旧文档没有恢复。原有正文已经覆盖大部分建议，本节补齐普通查询退出事实重放、公共契约依赖方向和跨报告实施顺序；命令入口统一的详细补充见[第二份报告第 11 节](report-02-tools-channels-http-sse-2026-11-02.md#11-最终复核追加统一命令体系与公共契约)。

### 12.1 八类架构的覆盖与处置

| 优先级 | 建议退出的架构 | 原有覆盖 | 最小替代 |
|---|---|---|---|
| P0 | canonical 自制存储引擎，包括两代日志、marker、摘要链、fence、poison 与应用恢复器 | 本报告第 4、7、8 节已覆盖 | SQLite WAL、短事务、必要索引与明确错误 |
| P0 | 普通业务查询与订阅恢复依赖全量事实重放 | 本报告第 6、7 节已有原则；本节补齐调用链和退出标准 | 业务表直接查询、owner 当前视图、有界 SSE 回放尾 |
| P0 | agent-loop 执行证明链与独立通用框架包装 | 本报告第 5 节已覆盖 | owner 直接管理请求、实际任务、取消与完成消息 |
| P1 | 三个公共频道、平行快照与重复事件分类 | 第二份报告第 2、5、7、8 节已覆盖 | 一份 SessionState、一条会话 SSE、类型化事件 |
| P1 | service、channel command、unified command 三套命令机制并行 | 原有报告未完整列出三套入口；第二份报告第 11 节补齐 | 一份类型化操作目录与生成契约，统一处理规则 |
| P1 | 投递失败后隐式重建、换 writer 或换存储的通用兜底 | 本报告第 6 节已覆盖 | 返回具体错误；启动、关闭和取消使用明确生命周期操作 |
| P2 | 普通正文强制内容寻址、外置和重复哈希的架构 | 本报告第 6、7、8 节已覆盖 | 普通内容直接入库，大附件按真实访问需求管理 |
| P1 | 客户端与契约依赖 session，session 再依赖执行核心的结构 | 原有报告未明确展开；本节补齐 | 独立于存储和执行实现的 Ringing 公共契约 |

优先级用于组织改造，不代表对应功能已经删除。P0 要退出的是决定系统复杂度的职责，不能只删除几个类型名称后继续在新模块里实现相同机制。

### 12.2 普通查询退出事实重放

当前存在两条已确认路径：

- [metadata 读取](../crates/qaqh-session/src/meta.rs#L48)读取会话事实并重建 `ProjectionSet`，再提取 metadata。
- [SSE replay_after](../crates/qaqh-ringing-runtime/src/v2.rs#L1194)读取会话事实，对历史逐条执行投影，然后才跳过游标之前的事实。

采用 SQLite 后，会话列表与 metadata、消息分页、工具结果、待处理交互直接查询业务表。订阅恢复只读取当前 epoch 的有界内存回放尾；不能覆盖时取状态快照。不能将事实换成数据库行，再保留原有全历史重放读取路径。

模型上下文与 UI 仍各自构建需要的视图，业务变化也仍可产生事件。删除的是“普通读取必须先经过通用事实重放”的约束。历史分页不等于一次加载全部历史；SessionState 也不能成为把整个数据库常驻内存的新理由。

### 12.3 公共契约从存储与执行实现中独立

当前依赖关系有明确证据：

- [qaqh-client](../crates/qaqh-client/Cargo.toml#L13)直接依赖 `qaqh-session`。
- [qaqh-ringing-contract](../crates/qaqh-ringing-contract/Cargo.toml#L14)依赖 `qaqh-session`，其[客户端状态定义](../crates/qaqh-ringing-contract/src/lib.rs#L8)直接使用 session 的投影、事实及任务看板类型。
- [qaqh-session](../crates/qaqh-session/Cargo.toml#L14)依赖 `qaqh-agentloop-core`。

依赖箭头表示调用方依赖被调用方。目标是让客户端、daemon、session 存储与 agent-runtime 都使用现有 Ringing 契约中的公共命令、状态和事件类型；公共契约不依赖数据库实现、session 投影实现或执行循环。

真正共享的业务身份和内容类型可以来自中立模块。存储层仍可使用自己的行结构和 SQL 映射，但不把持久化实现类型直接导出成客户端必须理解的协议。

不为修正这个方向新建一排 facade、adapter、Repository trait 或公共循环框架。`qaqh-session` 可以保留明确的数据访问职责，`qaqh-agent-runtime` 保留执行职责；`qaqh-agentloop-core` 的必要实现并入 runtime 私有模块，执行证明体系退出。判断标准是职责和依赖是否消失，不以 crate 数量或删行数作为成绩。

### 12.4 保持功能的简化与主动放弃的能力

合并重复状态、统一公共契约、直接查业务表、移除同进程重复摘要和票据，目标是保持已有业务功能并缩短实现路径。权限、沙箱、完整输入、工具结构化结果与 effects、任务句柄、后台进程管理、取消和有界队列仍需有效。

应用层日志链完整性证明、自动外部副作用对账与续跑、跨重启中间进度重放，以及可选的跨会话内容去重和自动 GC，是明确放弃或待按实际用途取舍的能力，不能宣称删除后所有保证完全等价。具体边界沿用第 8 节；未穷尽审查的模块不以“没有用途”为由直接定性。

### 12.5 两份报告统一后的执行顺序

1. 先确定要保留的业务语义和 Ringing 契约，取消对存储及执行实现类型的依赖；公共字段、命令回执和重试边界按明确决定调整。
2. 建立 SQLite WAL 业务存储与简单会话 owner，接通完整输入、工具输出、结构化结果和 effects。
3. 让实际调用者退出 canonical 自制存储、全事实查询、重复投影和执行证明链，然后删除旧框架与导出。
4. 完成前后端契约切换，统一命令处理与会话状态/SSE，删除旧频道、旧入口与客户端分流代码；不留长期兼容转换层。
5. 清除失效配置、注释和专属测试，只验证仍然提供的实际功能与数据库、执行、快照交接边界。

实现时让每一项能力有明确的接手路径，不能先把工具结果、取消或后台进程管理删除，再以“简化”为由接受功能失联。业务存储仍只有 SQLite 一份权威，旧数据按明确决定一次性转换或退役。

原第 11 节的“下一轮”描述的是当时审查阶段；后续协议分析见第二份报告及其追加章节。尚未确定的完整字段、schema、Rust 数据库库与回执细节仍未冻结。本次最终复核仅追加文档，没有实施运行代码改造。
