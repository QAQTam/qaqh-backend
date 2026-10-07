# AGENTS.md — QAQ-Harness 后端工程宪法

> 适用于在本仓库工作的**所有**人类与模型。动手前必须通读本文件。
> 本文件规定"程序该怎么设计"，测试规矩只是其中一节。与任何 plan/spec 冲突时，以本文件为准；
> 要修改本文件，必须由仓库所有者批准。

当前施工单：[`docs/spec-architecture-convergence.md`](docs/spec-architecture-convergence.md)（下文简称"收敛 spec"）。

---

## 0. 三句话

1. **会话只有一个事实源**：canonical log（`events.jsonl`）+ 会话 blob 存储。其他一切都是可以删掉再重建的投影。
2. **状态显式传递**：会话相关的状态不允许放在全局变量或线程局部变量里。
3. **删掉旧的，而不是并存**：每一条迁移都必须有结束条件，结束时旧路径必须删除。

---

## 1. 架构不变量（违反任何一条 = PR 不可合入）

### 数据与持久化

- **I1 唯一事实源。** 会话状态的权威只有 canonical log 和会话 blob 存储。其他持久文件都必须在
  `docs/ARCHITECTURE.md` 的存储表中登记为"投影 / 缓存 / 诊断 / 运行态"之一，并写清重建来源。
  未登记的新持久文件不允许出现。
- **I2 写入顺序。** blob 落盘（fsync）→ fact 追加（fsync）→ 更新投影 → 对外发布。
  对外发布的东西必须已经持久化；唯一的例外是 live 帧（流式增量），它本来就允许丢失。
- **I3 只追加，不改写。** 任何日志都不允许重写或截断。要更正，就追加一条新 fact（例如 undo 写的是
  `ContextRewound`，而不是重写归档）。
- **I4 fact 不携带累计或派生的文本。** 流式增量永远不落盘。（教训见 `crates/qaqh-runtime/src/timeline_store.rs:4-19`：
  累计文本写进 append-only 日志后，单个会话膨胀到 829 MB。）
- **I5 ContentRef 必须能解析。** fact 中出现的每个 `ContentRef`，在会话存续期间都必须能从 blob 存储取回原文。
  禁止把哈希当指针用，禁止引用带 TTL 的缓存。追加时由 `SessionLedger` 强制校验。
- **I6 兼容代码只放在迁移模块。** 读取路径只认当前格式。旧格式的识别和转换只能写在 daemon 启动时运行的
  `migrate` 模块里；该模块幂等，执行后写入 `data_version` 标记。禁止新增 migrate-on-read。

### 事件与副作用

- **I7 会话 actor 只有两种输出**：fact（持久化，经 `SessionLedger`）和 live 帧（易失，经 `Emitter`）。
  不允许出现第三种事件词汇。
- **I8 副作用只能由 fact 或显式调用触发。** 禁止通过对遥测或广播事件做模式匹配来触发业务副作用
  （反例：`crates/qaqh-runtime/src/actor.rs:59-74`）。
- **I9 每条客户端命令都必须有终态。** 终态要么来自 fact 折叠，要么在 ack 时同步给出。禁止用 TTL 过期来充当终态。

### 状态与并发

- **I10 不用环境状态。** 禁止为会话级数据新增 `static` 可变量、`thread_local!`，或"先读 TLS、再回落全局"的结构。
  会话上下文通过 `SessionScope` 显式传入。允许存在的进程级 static 只有：HTTP client、tokio runtime handle、
  日志、编译期常量表。新增条目需在 `docs/ARCHITECTURE.md` 登记并说明理由。
- **I11 锁的纪律。** 持锁期间不允许做 IO、阻塞的 channel send、`block_on`，也不允许回调其他组件。
  需要做这些事时，先复制出需要的数据，释放锁之后再做。
- **I12 取消只有一种机制**：`CancelToken` 树（daemon 为根，会话、turn、工具调用逐级为子节点）。
  禁止新增取消旗标。

### 分层与边界

- **I13 依赖方向固定**（上层可以依赖下层，反之禁止）：
  ```
  qaqh-types → qaqh-gate / qaqh-message → qaqh-session → qaqh-runtime → qaqh-daemon
             → qaqh-domain → qaqh-ringing ↗
  工具族：qaqh-fs-core / qaqh-permission / qaqh-policy / qaqh-sandbox → qaqh-tool-core → qaqh-*-tools → qaqh-workspace
  ```
  下层不允许知道 wire（`qaqh-ringing`）或 runtime 的存在。新增 crate 间依赖边，要同步更新 `docs/ARCHITECTURE.md`。
  禁止用"运行时注册 `fn` 钩子"来绕过依赖方向（反例：`crates/qaqh-permission/src/lib.rs:187`）。
- **I14 Ringing v2 wire 冻结。** 只允许新增带 `#[serde(default)]` 的字段。新增路由、删除字段或改变语义，
  都必须先在 `docs/adr/` 写 ADR 并获得所有者批准。
- **I15 provider 差异只能来自配置。** 禁止按模型名、URL 子串做特判；差异通过 `EndpointSpec` 表达。

### 代码形态

- **I16 一个概念只有一个名字。** 会话标识统一叫 `session_id`，不叫 `seed`（唯一例外是 gate 中外部 provider
  的 `session.seed` 请求头）。标识符描述"是什么"，不描述"什么时候加的"：当前代码中禁止出现
  `v1/v2/new/old/legacy` 这类命名（迁移模块除外）。
- **I17 注释写现在，历史写进 git。** 注释只说明当前行为和原因。禁止在新代码注释里写 BUG 编号、
  "阶段 3d""PR-3-4""§4.0.5 迁移完成"这类变更日志；这些内容放进 commit message 或 ADR。
  回归测试的测试名或 doc 可以引用 bug 编号。
- **I18 参数超过 5 个就改用结构体。** 禁止新增 `#[allow(clippy::too_many_arguments)]`（现存 50 处只减不增）。
- **I19 文件体积。** 被修改的文件超过 1500 行时，本次改动不得让它再变长；新文件不得超过 1000 行。
- **I20 不允许静默丢数据。** 会导致用户数据丢失的路径必须返回 `Err` 并上报，
  不能只 `log::warn!/error!` 后继续执行（反例：`crates/qaqh-message/src/store.rs:788-795`）。
- **I21 crate 边界的错误要有类型。** 新增的 `pub` API 不允许返回 `Result<_, String>`，使用具名 error enum。

---

## 2. 工作流程规矩

- **P1 先读后写。** 动工前读：本文件、`docs/ARCHITECTURE.md`、收敛 spec 中对应的任务节。
  在 PR 描述里写明本次涉及哪几条不变量，以及如何遵守。
- **P2 一个 PR 对应一个任务 ID**（例如 `T2.3`）。禁止顺手修改范围外的东西；发现的问题记到收敛 spec 的
  §9"发现记录"，不要就地改。
- **P3 spec 与代码事实冲突时，停下来。** 写一条发现记录说明冲突点，等裁决，禁止即兴设计。
  spec 中引用的行号可能已经漂移，以符号为准；如果符号也找不到，同样算冲突。
- **P4 迁移窗口必须有终点。** 双写、shadow、兼容开关只能出现在 spec 明确声明的窗口内，并且必须写明删除任务的 ID。
- **P5 完成的定义**（全部满足才能说"完成"）：
  ```
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace -- --test-threads=1
  ```
  外加任务验收条款里列出的探针或测试。跑不了的项目必须明确写"未验证：<原因>"，禁止声称通过。
- **P6 结构性变更与文档同步。** 改变分层、存储、事件或线程模型的 PR，必须在同一个 PR 里更新 `docs/ARCHITECTURE.md`。

---

## 3. 测试规矩（服从于上面的设计规矩）

- **测接缝，不测实现。** 首选的测试形态是"fact 序列输入 → 投影 / 上下文输出"和"命令输入 → fact 输出"。
  禁止写断言"旧桥还在"或"某个内部字段存在"这类锁死实现的测试。
- **修 bug 先写红测试。** 先写出能复现的失败测试，再修复。
- **金标语料**（`crates/qaqh-session/tests/golden/`）是上下文等价性的唯一裁判，收敛 spec 的 T2.3 建立它。
  修改投影逻辑时必须全量通过。
- **测试里不碰进程全局状态。** 需要会话上下文就构造 `SessionScope`。依赖 `--test-threads=1` 才能通过的新测试，
  视为违反 I10。
- 测试代码允许 `unwrap`（见 `clippy.toml`），生产代码禁止（`Cargo.toml` 中 `unwrap_used = "deny"`）。
