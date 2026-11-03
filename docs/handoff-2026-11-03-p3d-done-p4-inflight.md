# Handoff — 2026-11-03:P3-D/P4/P5 已提交；完结审计补齐

## 0. 使命与standing directives(不可变)

- **目标(goal-mode)**:"按照既定计划以及文档把全部工作安排妥当并且写测试;且时刻注意硬件资源占用问题后,帮我直接完成全部的工作"——按 docs/report-01/02/03 三份报告完成整个架构简化程序,写测试,自主推进。
- **用户既定决策**:旧数据不要了(无迁移);alpha 不做确定性兼容;你直接全改,前端我自然有办法。
- **宪法(AGENTS.md)要点**:#7 开工查 disk/mem/cpu 并预告卡顿;#8 工具失败不得声称完工;#9 RingingV2 协议与命令面是最高准则,禁止转换层懒政;#10 不许每改一次跑全仓测试(磁盘);#11 `rg -r` == `--replace` 陷阱;#12 sed 改完必须 rg 验证;#13 用 todo 工具;#14 新库用 find-docs。
- **资源现状(本次开工时)**:磁盘 86G 空闲 / 内存可用 ~4.5G / 8 核。cargo check --workspace --tests 约 2-4 分钟,可行;**不要跑 `cargo test --workspace`**(用户两次拒绝过),只跑逐 crate 定向测试。

## 1. 已完成(全部已提交,按序)

| commit | 内容 |
|---|---|
| 2685453d | RingingV2 契约独立 + SQLite 存储基座 |
| ec744693 | SessionState 业务表写读往返 |
| 7a87f41a | 会话 owner 接管工具生命周期 + E09/E10 + effects 消费 |
| 439c423f | owner 驱动的新回合循环(证明链外) |
| 3e05a105 | registry 切 SessionLoop,SSE/状态单权威落 owner |
| **78b77876** | **P3-D:旧存储/证明链/投影面全量退役(140 文件,-34342 行)** |

P3-D 具体内容(78b77876):
- 删除:qaqh-agentloop-core 整 crate、qaqh-session canonical/projection/session_commit/commit_state/context_projection/meta/store/wire/team 旧存储、ringing-runtime v2.rs/agent_graph.rs、agent-runtime activity.rs/liveness.rs/session_agent 旧文件。
- SQLite 单权威:sessions 表补 `model_selection`/`custom_tools_json`/`skills_json`/`frozen_annotation` 列(schema v2,SCHEMA_VERSION=2,旧库直接删);SessionRow/SessionListRow 全列 + turn_count;`session_row_with_turn_count()` 供 SessionMeta 组装。
- 创建流:`handlers/session.rs` 的 `create_business_row()`(CreateRowInput)一次落库,无 meta.json 中间态;`SessionManager::allocate_session()` 无参返回 `Result<String,String>`(`ses_{ulid}`);materialize_canonical_session* 全删。
- cwd 读取唯一口:`registry::spawn::session_cwd(owners, session_id) -> Option<String>`(read_blocking + session_row);`ensure_session_workspace(owners, ...)`;fs_git/plan/stats/service.list_sessions 全切 owners。
- stats::activity 直查 tool_calls 终态表(status: finished/failed/cancelled/backgrounded)。
- registry 不再持有 SessionManager(`AgentRegistry::new(owners)`);OwnerDirectory 有 `set_ephemeral()`。
- pending_store 删掉 causation index/error_code_of(回执终态在派发时直写)。
- contract-export 切契约类型(SessionBootstrap/SessionState/SessionEvent/SessionEventFrame),index.ts 无泛型别名层;fixture 名 `event-frame-interaction.json`。
- qaqh-client 导出 `contract_events`/`contract_state`(v2.rs);ClientV2Event = SessionEventFrame,cursor = `"epoch:seq"` 字符串。
- 集成测试:remote_fs_allowlist 种子改走业务行(service.owners().db().write_blocking + store::create_session);v2_public_api 匹配 SessionEvent 变体;删 tool_crash_recovery/v2_acceptance_matrix/e2e_seed。
- **验证状态:全仓 `cargo check --workspace --tests` 0 错误 0 警告;定向测试全绿**:qaqh-session 12+36(db)、agent-runtime 36、ringing-runtime 9、runtime lib 49 + 集成 13、client 68、contract-export 6、daemon 31、types/ringing/domain/workspace ~240。

## 2. 当前状态(P4/P5 完结；最新收尾状态见 §9)

P4 按 report-03 完成三入口统一与四项决策。当前命令契约 revision 为 `ringing-commands-6`。

1. **唯一命令根**:业务写命令统一为 `qaqh-ringing::RingingCommand` + `RingingV2CommandEnvelope`。HTTP 写入口为 `POST /ringing/v2/commands`;daemon 的 `unified.rs` 已并入 `commands.rs`。SDK 的 `Client::send_command(target, command, options)` 是唯一写入口，`ActionRequest`/`Client::action` 已删除。ServiceMap/service decoder 仅保留查询与账户 RPC，不再声明另一套配置、会话、workspace、todo、plan 等写命令。
2. **写命令执行**:配置/profile/workspace/subagent/todo/plan/diagnostics 由公共 RingingCommand 直接分派到类型化 handler；不经过服务字符串解码或转换层。无消费者的旧 Conversation 操作与 compact probe 已删除。
3. **会话与 settings**:创建会话只保留一个业务实现与 `SessionCreateResult`。`session.settings.patch` 要求 `settings_revision`，并在 SQLite `UPDATE ... WHERE settings_revision = ?` 处 CAS。旧 service 写入目录已删除；archive、title、workspace 等非 settings 字段不递增设置版本。活跃会话 settings patch 通过单个 `AgentReloadConfig` 刷新 provider、工具目录和执行策略，初始 spawn 与 reload 共用 session tool mode 映射。
4. **报告中发现的声明缺口**:`ToolInvoke`、`ProviderModelsDiscover`、无消费者的 SessionCommand/旧 conversation 操作已从公共目录删除；账户 RPC 保留在 ServiceMap/service 表。
5. **完成标准**:同一操作一份字段真源+一个实现；每个已公开命令有执行分支；创建结果/归属明确；settings revision 在写边界检查；contract exporter 不导出第二代命令目录。

## 3. 执行顺序

1. **P4-a/c/d 已完成**:DomainCommand/重复领域类型退出；业务事件字段完整进入 SessionLoop 与 timeline；会话创建实现归并。
2. **P4-b 已完成**:唯一命令目录、统一 envelope、SDK 写入口和直接 handler 分派已完成；全局 ops 不进入 session actor。
3. **P4-e 已完成**:活跃 settings 写入只有 revision CAS 命令入口；元数据 patch 中 title/archive/workspace 不属于设置字段。
4. **P5 收尾与完结审计已完成**；最终改动和验证见 §9。
5. 只跑受影响 crate 的检查与定向测试；**不跑全仓测试**。

## 4. 关键 API 速查(本会话固化的事实)

- `qaqh_session::db::store`:`NewSession`(含 model_selection/custom_tools/skills/frozen_annotation/ephemeral)、`create_session(conn,&NewSession)`、`session_row(conn,id)->Option<SessionRow>`、`session_row_with_turn_count(conn,id)->Option<(SessionRow,i64)>`、`list_sessions(conn,include_archived,limit)->Vec<SessionListRow>`、`SessionColumnPatch`(model_selection: Option<SettingValue<ModelSelection>>,Use 落 JSON/Inherit 置 NULL)、`update_session_columns(conn:&mut Connection,...)`。
- `OwnerDirectory`:`create_session(NewSession) async`、`ensure_session(id,parent,ephemeral)` sync、`set_ephemeral(id,bool)`、`session_parent/children/run_state`、`db()->&Arc<Db>`;`Db::write/read(async)+write_blocking/read_blocking(sync)`;read_blocking 闭包返回 rusqlite::Result(Ok 类型即返回 T,不加嵌套 Result)。
- `SessionLoop::new("SES-ID", owner, port, ProviderRequestSpec, workspace_root)`;`send_input(LoopInput{input_id,text,purpose,as_system,attachments,images,inter_agent,subagent_terminal})`、`cancel_turn(Option<&str>)`、`respond_interaction/respond_permission/abandon/reload_provider`。
- 测试造会话:`SessionManager::new_for_test(dir, active)`(现在会自建目录)+ `allocate_session()` + `owners.db().write_blocking(create_session)` 或 owners.create_session(...).await。
- 会话 owner 泵/广播:`owner.subscribe() -> broadcast<SessionEventFrame>`;SSE cursor `"epoch:seq"`;gap→resync_required。
- 契约:qaqh-ringing-contract{commands,events,fact,state,team,ulid};`generate_ulid()/causation_for_command()` 在 contract::ulid;SessionEvent `#[serde(tag="type", rename_all="snake_case")]`;SessionEventFrame{seq, event(flatten)};ContentValue tag=kind/content=data。
- Id 类型构造:`SessionId::new(text)`、`InteractionId::new(text)`、`AgentPath::parse_absolute(text)`、`MessageId::new(text)`。

## 5. 陷阱(踩过的)

- rusqlite 多闭包共享 `&mut values/sets` 会 E0499——要么一个闭包包揽,要么 if-let 内联;`session_meta.rs` 读取链用 `read_blocking(...).ok()? ?;`(store helper 已消除类型标注需求)。
- `qaqh-runtime` 无 rusqlite 直接依赖——闭包里别写 `rusqlite::Error` 字面类型,用 helper fn 或 `_`。
- write_blocking 闭包是 `move`,&str 捕获要在闭包外 `.to_string()`。
- `Option<Result<Option<T>>>.transpose().map(...).flatten()` 处理 Use/Inherit 双层 Option。
- schema v2 与 store.rs 列必须同步加(本次 model_selection 差点只加了一半)。
- 用户拒绝全仓 cargo test;check --workspace --tests 可以。

## 6. 环境注意

- 工作目录 /home/qaqtamsy/Projects/qaqh-backend-v3,分支 v3,remote 无(本地提交)。
- 本文记录了 P4/P5 分阶段执行过程；继续续作时以 §9 的最终审计状态为准。

## 7. 2026-11-03 continuation progress

本节记录续作过程；当前状态以 §2、§3 为准。

### 已完成的续作

- 基线提交为 `221e7c66 docs: P3-D 完成后的 handoff(P4 执行清单/陷阱/API 速查)`；本轮改动尚未提交。
- P4-a:删除 `DomainCommand`、`channel()`、domain 导出、Ringing `From<DomainCommand>`，并更新 domain crate 与 command module 的旧四层/Agent core 声明。
- `PublishResourceChanged` 已从 domain 与 ringing-contract 的公开命令目录删除；源码只剩 task handler 的说明注释。
- 无生产分发实现的 `ToolInvoke` 与明确 deferred 的 `ProviderModelsDiscover` 已从公开目录删除；ToolInvoke 的回环测试改为覆盖仍受支持的 `ToolPermissionRespond`。
- 会话创建归并:contract `SessionCreateResult` 位于 `qaqh-ringing`；`create_session_command()` 直调唯一 `create_session()` helper，不再重装旧参数 DTO，并把新会话 attach 到当前 lease。
- service decoder 与 service method table 中的 `session.new` / `session.create` 创建入口、参数 DTO 和旧 fixture 已移除；contract exporter 导出 `SessionCreateResult` 与 `session-create-result.json`。
- RingingV2 `session.create` 命令包含旧 service DTO 中实际使用的 `profile` / `effort`；契约代际已递增。
- P4-e settings CAS 已落到 SQLite 写边界：schema v3 增加 `settings_revision`；所有通过 `session_meta::patch` 改动设置的写入递增该版本，统一 `session.settings.patch` 在同一条 `UPDATE ... WHERE settings_revision = ?` 中 CAS 并回传新版本。`SessionMeta` / 列表查询暴露版本；缺失前置条件在契约校验拒绝，旧版本写入返回 HTTP 409 `settings_revision_conflict`。按“旧数据不迁移”决定，schema v1/v2 会明确报 `SchemaTooOld` 并要求删除旧数据库重建。
- 不存在运行时消费者的旧 `skills.operation` SDK ActionRequest、service decoder 与 domain 命令已删除；它曾声明 expected revision 但并未执行。
- P4-d 字段保留：`ConversationSendMessage` 直接使用契约 `InputPurpose`；registry 将图片、`inter_agent` 和 `subagent_terminal` 传到 `SessionLoop`；owner timeline 持久化三个字段，`as_system` 作为 System actor 写入并在 provider context 中生成 system 消息，图片作为图片块进入模型上下文。QueueOnly/Steer/Interject 的排队与安全点调度原已由 `SessionLoop::send_input` 承担。
- 命令元数据和输入用途已删除 domain 中的重复类型定义，改用 `qaqh-ringing-contract` 类型；domain 的 TS feature 同时启用该契约的 TS 导出。
- P4-b 清理：移除 `qaqh-ringing-contract::commands` 中无消费者的 `SessionCommand` / `SessionCommandEnvelope` / `InteractionAnswer` 目录；domain 的 `AskAnswer` 改为直接使用同一契约类型。
- 删除仅声明但没有执行分支的旧 Conversation 命令 `undo_turn` / `compact` / `load_more` / `set_mode` 及 `ConversationMode`，并移除 `load_more` 的专门拒绝分支。
- P4-b 收口：所有公共写操作统一到 `RingingCommand`，daemon HTTP 入口固定到 `/ringing/v2/commands`；旧 per-channel POST 入口与并行 UnifiedCommand 类型移除。ServiceMap/service decoder 只保留查询/账户 RPC。
- 移除未实现的 `SetToolMode` 公共命令。session settings patch 落库后只发送 `AgentReloadConfig`；reload 从业务行读取 session tool mode/custom tools，并同步重建 provider 工具目录、session/global allowlist、执行策略与 loop cwd。root 初始 spawn 使用同一 tool mode 映射。
- P5 命名清理：`create_session_unified()` 改名为 `create_session_command()`；contract fixture 改为 `session-create-result.json`。
- `SessionAttach` 在统一写入口恢复为 View scope；旧 session-close fallback helper 与对应孤立测试已删除。

### 验证

- `cargo check -p qaqh-domain -p qaqh-ringing -p qaqh-ringing-contract --tests` 通过。
- `cargo test -p qaqh-domain -p qaqh-ringing -p qaqh-ringing-contract -p qaqh-contract-export` 通过：domain 20、ringing 72、ringing-contract 117、contract-export 6。
- `cargo check -p qaqh-runtime -p qaqh-daemon --tests` 通过。
- `cargo test -p qaqh-runtime -p qaqh-daemon` 通过：runtime 49、daemon 30；相关 integration tests 全绿（daemon 1，runtime 13）。
- P4-e 本轮：`cargo test -p qaqh-client -p qaqh-session -p qaqh-ringing -p qaqh-runtime` 中 client 68、ringing 21、runtime 62 全绿；session 的 CAS 回归测试先暴露 `update_session_columns` 占位符偏一位，修正后 `cargo test -p qaqh-session` 10/10 通过。
- `cargo check -p qaqh-daemon --tests` 通过。
- P4-d 定向验证：`cargo check -p qaqh-domain -p qaqh-ringing-contract -p qaqh-agent-runtime -p qaqh-runtime -p qaqh-subagent -p qaqh-client -p qaqh-session --tests` 通过；`cargo check -p qaqh-domain -p qaqh-ringing-contract --features ts` 通过。
- `CARGO_BUILD_JOBS=4 cargo test -p qaqh-domain -p qaqh-ringing-contract -p qaqh-ringing -p qaqh-agent-runtime -p qaqh-runtime -p qaqh-subagent -p qaqh-client -p qaqh-session -p qaqh-contract-export` 通过：agent-runtime 13、client 62 + integration 6、domain 14、ringing 74、ringing-contract 117、runtime 83 + integration 13、session 12、subagent 14、contract-export 6。之后新增的 timeline 旧 JSON 与 P4-d 输入上下文定向用例各 1/1 通过。
- 删除未接线 `SessionCommand` 目录与旧 Conversation 操作后的 `cargo test -p qaqh-domain -p qaqh-ringing-contract -p qaqh-contract-export` 通过：domain 13、ringing-contract 115、contract-export 6。
- 再次移除旧 Conversation 操作后，`cargo check -p qaqh-domain -p qaqh-ringing-contract -p qaqh-ringing -p qaqh-client -p qaqh-tool-core -p qaqh-daemon --tests` 与 `git diff --check` 通过。
- ConversationMode 清理后的最终定向测试：`cargo test -p qaqh-domain -p qaqh-ringing-contract -p qaqh-ringing -p qaqh-contract-export` 通过（domain 12、ringing-contract 115、ringing 74、contract-export 6）；同一轮 `cargo check -p qaqh-domain -p qaqh-ringing-contract --features ts` 通过。
- `git diff --check` 通过。
- `cargo fmt --all -- --check` 当前不干净，输出包含 workspace 既有格式差异（含 `/home/qaqtamsy/Projects/mutil-ai` 与多个未改源码）；没有对整个 workspace 执行格式化，避免引入大批无关改动。
- 未运行 `cargo test --workspace`。

### 最新续作验证

- `CARGO_BUILD_JOBS=4 cargo check -p qaqh-domain -p qaqh-ringing-contract -p qaqh-ringing -p qaqh-agent-runtime -p qaqh-runtime -p qaqh-subagent -p qaqh-ringing-runtime -p qaqh-client -p qaqh-daemon -p qaqh-contract-export --tests --examples` 通过。
- 定向测试：agent-runtime 14；client 56 + lease-renew 2 + public API 2 + v2 API 3；ringing 23；runtime 49 + integration 13；contract-export 6，全部通过。
- Daemon 初次定向运行发现统一 `/commands` scope 表漏了 `SessionAttach => View`；修复并删除遗留 helper 后 `cargo test -p qaqh-daemon --bin qaqh-daemon authz_matrix_tests` 12/12 通过。daemon 其他 17 个单元测试在首轮均通过。
- `python scripts/check-sdk-contract.py --typescript` 通过，contract revision `ringing-commands-6`。
- workspace root reload 收口后，`CARGO_BUILD_JOBS=4 cargo check -p qaqh-agent-runtime -p qaqh-runtime -p qaqh-daemon --tests --examples` 与 `cargo test -p qaqh-agent-runtime` 通过（14/14）。session-create 命名与 fixture 调整后的完整 SDK TypeScript contract script 也通过。
- 本续作触及的 Rust 文件定向 `rustfmt --check` 通过；全仓 `cargo fmt --all -- --check` 仍报告其他文件/外部依赖中的既有格式差异。`git diff --check` 与代码 stale-route/name 检索通过。

### 后续说明

P4/P5 已提交到当前分支。继续工作前先检查最新 `git log` 与工作树；不要运行 `cargo test --workspace`。

## 8. 2026-11-03 完结审计补齐

续作从 `1a0dec23 refactor: unify RingingV2 command surface` 的干净工作树开始。按三份报告和源码逐项核对后，移除了此前未收净的依赖、频道快照和 JSON workspace 注册表：

- `qaqh-client` 不再依赖 `qaqh-session`；事实字段类型从独立 `qaqh-ringing-contract::fact` 导出。
- 从 `qaqh-ringing` 删除未接运行路径的三频道 `RingingV2Bootstrap`/快照包装、`stream_key` 事件 envelope、canonical cursor 和旧 reset 类型；SDK 不再公开旧 channel 事件类型与别名。当前 bootstrap/event 只使用 `SessionBootstrap`/`SessionState` 和 `SessionEventFrame`。审批响应直接复用契约 `InteractionKind`，不再维护 RingingV2 副本。
- workspace 注册与 session 归属从 `workspaces.json` 移入同一 SQLite 数据库的 `workspaces`/`workspace_sessions` 表，读写经过现有数据库执行器；DTO 移到 Ringing 公共契约，文件浏览和 session 列表直接查询数据库。`SCHEMA_VERSION` 升至 4；依照既定无迁移决策，旧 schema 数据库需删除重建，旧 `workspaces.json` 不导入且不再读写。
- `fs.list`/`fs.read` 的允许根查询不再吞掉 SQLite 错误并继续运行。

验证：

- 串行 `cargo check` 通过：`qaqh-session`、`qaqh-ringing-contract`、`qaqh-ringing`、`qaqh-client`、`qaqh-runtime`、`qaqh-daemon`、`qaqh-contract-export`。
- `python3 scripts/check-sdk-contract.py` 通过；contract-export 定向测试 6/6，导出与 manifest 校验通过。
- `cargo test -p qaqh-client --test v2_public_api` 通过（3/3）。
- `cargo test -p qaqh-runtime --lib workspace_list_entries_are_typed_at_the_boundary` 通过（1/1）。
- 未运行 `cargo test --workspace`。本次执行时可用内存约 1.2 GiB、swap 接近满载，所有构建串行执行。

## 9. P4/P5 最终收尾审计

### 完成项

- 删除 `qaqh-domain` 与 `session_fact_v2` 投影层，旧事件、三频道 envelope、客户端别名和 qruntime facade 改为直接使用 Ringing 契约类型。
- `RingingHub` 不再维护 JSON/BIN 内容 sidecar、TTL、pin/refcount/GC；二进制内容进入 session SQLite 的 `session_contents`，`SCHEMA_VERSION=5`，会话行删除时由 FK 清理内容。旧 schema 按既定决策直接删库重建。
- 上传响应解析为 `RingingV2ContentUploadResponse`，SDK 返回规范 `ContentRef("sha256:<hex>")`；附件预览、SDK 下载和 subagent 下载均按该 hash 读取。
- 任务描述、产物和完成结果可能由子 session 产生；写入 team task 时，在同一个 SQLite transaction 内将内容复制到 root session，确保任务引用随 root 的生命周期可读。
- RingingV2 reset response 由 wire crate 直接导出；移除旧 discovery fallback、旧 v2 capability `drive` 和旧 ACK 兼容用例。TypeScript 导出中将事实层 `ActivityState` 与服务查询的 `SessionActivityState` 恢复为不同名称。
- 删除缺少源码且指向已删除 crate 的 memory hotspot probe Cargo manifest/lockfile；保留 `results-2026-10-22.txt` 作为历史记录。
- 同步本 handoff 的 schema 版本与最终验证状态。

### 最终验证

- 串行 `CARGO_BUILD_JOBS=1 cargo check --tests -p qaqh-session -p qaqh-runtime -p qaqh-daemon -p qaqh-ringing-runtime -p qaqh-client -p qaqh-agent-runtime -p qaqh-contract-export` 通过，无 warning；只编译测试目标，没有执行这些测试。
- `python3 scripts/check-sdk-contract.py --typescript` 通过；contract-export 自检 6/6，导出/manifest 与严格 TypeScript consumer 检查通过。该脚本固定会执行 contract-export 的 6 个自检。
- 本轮 touched Rust 文件 `rustfmt --check --edition 2024` 与 `git diff --check` 通过；源码、Cargo manifest 和锁文件中没有 `qaqh-domain`、`qaqh-message` 或旧内容存储实现的引用。
- 未运行 `cargo test --workspace`，没有新增测试。
