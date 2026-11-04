# Backend v3 契约裁定(阶段 0)

> 后续待实施规范：[Exec 执行会话与工具状态统一修改 Spec](spec-exec-session-lifecycle-2026-11-04.md)。新实现切换时替代本文 D4 的工具状态事件、D6 的 exec 暂态恢复及 D8 的握手约定；D1 的 exclusive watermark 语义保留。该 spec 尚未实施，本注不表示当前运行契约已经切换。

日期:2026-11-03。基线:`b144398d`(干净)。依据 handoff `backend-v3-implementation-handoff-2026-11-03.md` 的"建议单独锁定的产品 / 契约问题"。实现者会话在用户离线期间按宪法 #3(以更优架构推行)做出以下裁定并执行;任何一项如需反悔,改动面在文中标注。

## D1(B01)水位语义:`seq` = "已覆盖序号上界"(exclusive)

- `seq` 是 epoch 内 0 基单调序号:第 s 个发出的事件帧携带 `seq = s`。
- `SessionState.seq`(bootstrap 水位)= **下一个将被分配的 seq**。等价表述:快照已覆盖全部 `seq < state.seq` 的事件。
- 客户端规则:bootstrap 后**跳过** `seq < state.seq` 的帧,**处理** `seq >= state.seq` 的帧;相等边界(首帧 `seq == state.seq`)必须处理,不得按 `<=` 丢弃。
- SSE 游标 = 客户端最后处理的帧 seq,格式 `epoch:seq`。
- **owner 快照定序**:先取 `current_seq()`(= next_seq 原子值)再读业务库。该顺序保证无静默丢帧:
  - 若事件在水位读取前发出(`T_emit < T_seqread`),其 DB 写必然更早(`T_db < T_emit < T_seqread < T_dbread`)→ 效果已在快照,客户端跳帧正确;
  - 若事件在水位读取后发出,帧 seq `>= state.seq` → 客户端必然处理,不丢。
  - 残余重叠(效果已在快照又收到同一帧)由客户端按 ID 幂等吸收:timeline 同 `block_id` 合并、tool/interaction/turn 按 id 替换——契约既有规则,不是新增要求。
- **SSE resync 判定**(daemon `handle_events_v2`):订阅后读 `owner.current_seq()` 作实时位置 P(纯内存原子,不再查库);游标存在但 **epoch 不匹配 → resync**;`since < P → resync(cursor_gap)`;`since >= P` 或无游标 → live。lagged receiver → `stream_lagged` resync(不变)。
- 测试必须覆盖边界相等:帧 seq 0、`since == P`(live 不 resync)、`since == P-1`(resync)。

## D2(B05)同步命令 receipt:保存并重放 result/operation/revision

- receipt 增加同步结果载荷 `sync_result: { operation, result(JSON), revision? }`;`dispatch` 成功路径与结果一起落 receipt(持久化策略不变:TTL 300s、4096 条、best-effort 文件持久化、稳定身份键 scope)。
- 重放面(两处,同源 receipt):
  - 同 `command_id` + 同指纹重发 → `accepted` ACK,`existing = CommandReceipt { state, sync_result }`;
  - `GET /commands/{id}` status → `RingingV2CommandStatus.sync_result`。
- 契约新类型 `RingingV2SyncResult { operation: String, result: Value, revision: Option<u64> }`;`RingingV2CommandStatus` 与 `RingingV2ExistingResult::CommandReceipt` 增加可空 `sync_result` 字段(serde default,旧持久化文件兼容读)。
- 诚实边界:`succeeded` 而 result 缺失(引擎转发类命令)时 `sync_result = None`,不得伪造。引擎类命令照旧 `accepted` 无 result。
- 敏感边界:回执只进 daemon 数据目录 receipt 文件(现状),result JSON 与 ACK 相同形状;session.create 结果只含 `session_id`,不含密钥材料。

## D3(B06)设置目录:providers.toml 为执行目录,legacy profile 保留但边界锁死

- **执行目录** = `providers.toml`(`default_providers_path`):`provider.list/upsert/delete`、`provider.model.upsert/delete`、`model.selection.set_default` 全部使用它 + `config_revision` CAS(现状已一致,补测试锁定)。`check_revision` 仅在显式携带前置条件时校验 → **revision 0 是合法首次写**,不是跳过检查。
- **legacy 目录** = `profiles.toml`(`default_profiles_path`):`profile.list`(service)与 `profile.create/set_default/apply/save_current/delete`(命令)继续存在,服务 `ensureAccountProfile` 登录链。**不承诺 `profile.update`**(词表里没有,后端也不新增;改 profile 的产品动作 = delete + create)。
- 两个目录 revision **永不互换**;`profile.*` 命令不携带 `config_revision`。`provider_delete`/`delete_model` 的目录守卫(被 profile/default_model_selection 引用即拒绝)是权威行为,补测试。
- **实现补充**:目录 CAS 冲突(`check_revision` 的 RevisionConflict)必须以 `QaqhError::Conflict` 上抛(409,与 settings_revision 同档);旧实现映射成 `Invalid`(502 operation_failed),把并发修订事实伪装成参数错误,已修复。legacy 文件的 default 指针在 config(`active_profile`),`profile.set_default` 不再触碰文件;`profile.delete`/`save_current` 同步摘除/落 legacy 文件条目(Config::load 会重新导入文件残留)。
- 反悔成本:若日后退场 legacy profile,删除 5 个命令变体 + `profile.list` service + Electron E27 入口即可,契约面不变。

## D4(B03)工具声明与块类别

- `MessageDelta` 增加 `block_kind: AssistantBlockKind`(reasoning|answer,由进度通道 `ProgressBlockKind` 映射);`text` 仍为累计替换。
- `ToolArgumentsDelta` 增加 `tool_name: String`;`text` 仍为累计替换的原始参数前缀。
- 新增事件 **`ToolDeclared { call_id, turn_id, tool_name, args }**:`declare_tool_call` 落库后发出;`args` 为最终参数(裸 JSON 对象或 null)。
- `ToolStarted` 增加 `tool_name: String`。
- 事件词表 13 → 15 种(+`ToolDeclared`、+`DriverChanged`);事件均有界(参数/文本以模型输出与工具输出为上界,输出尾窗沿用既有有界通道)。

## D5(B04)snapshot 数据来源

- `title`:从 `sessions.title` 权威读入 snapshot;`session.rename` 经 owner 新 API 落库并发出 `SessionMetaChanged { title }`(生产构造补齐)。
- `driver`:见 D7(daemon 内存席位,bootstrap overlay 填充;claim/release 发 `DriverChanged`)。
- `subagents`:snapshot 从业务库权威推导——`tool_calls.effects_json` 中的 `SubagentSpawned { child_session_id }` 提供 `parent_call_id`/`role`;子会话业务行(回合 outcome)推导终态 `status`;`result` 不填(客户端经子会话自身 bootstrap/team 查询)。
- `SubagentUpdated` 实时事件仅在 spawn 生效(ToolFinished 携带 SubagentSpawned effect)时由 owner 发出**初始视图**;子代理**终态推送明确标为非实时能力**,客户端使用既有查询(session.list/team/子代理 bootstrap)。不为填字段加推测状态。

## D6(B09)resync 语义:**明确允许丢失暂态**

- 裁定为二选一中的"明确允许丢失":resync/重新 bootstrap 只恢复**已提交**业务状态(timeline、工具终态、交互);未 sealed 的流式文本、未 declare 的累计参数、stdout/stderr 尾窗**不承诺恢复**,以新 bootstrap 的空暂态覆盖旧暂态。
- 理由:客户端(SDK/WebUI)现有 resync 路径已按此语义实现——收到 `ringing.resync_required` 清空游标与暂态缓冲、强制重新 bootstrap(SDK `watch` reset 路径;WebUI reducer 重建 turn/timeline 并清空未归属文本/工具参数缓冲)。后端补运行中尾窗恢复需要把流式热路径状态化,收益(断线瞬间不闪退)与风险(主显示路径回归)不成比例。
- 本裁定写进协议注释与本文档;SDK/Electron 侧验收以"清暂态 + 重建"为准。反悔成本:owner 增加有界 in-flight 注册表 + `ToolCallView` 增加 `partial_args`/`output_tail` 字段 + snapshot 覆盖逻辑(约 200–280 行,契约加字段)。

## D7(B02)driver 席位:统一命令 + daemon 内存席位

- `driver.claim` / `driver.release` 是统一命令面的真实命令,**此前运行时无实现**(worker 分发表落到 "does not implement")——在 daemon 同步 dispatch 就地实现,不再投 worker。
- 席位状态:daemon 内存注册表(per-session `{holder, epoch}`)。语义归属:holder 是 daemon 认证的 lease(`client_session_id`),lease 本身不跨重启 → 席位不落库、不跨重启是**正确**语义;重启后新 server_epoch,客户端自然重 bootstrap。SessionState.driver 由 bootstrap handler 从席位注册表 overlay 填充(view.rs 不感知)。
- 语义:claim——空位或 `holder == caller` → 接受(holder=caller,epoch+1);被占且 `stale_holder != holder` → `accepted=false reason="busy"`(返回当前 holder/epoch 供仲裁);`release`——`holder == caller` 或 `expected_epoch == current` → 释放(epoch+1,holder=None);否则 `accepted=false reason="not_holder"`。
- 事件:claim/release 生效即经 owner 发 **`DriverChanged { driver: Option<DriverState> }`**(owner 增加 `publish` 公共出口,daemon 侧生产者经 owner 定序)。
- coverage:`driver.claim/release` 改指 `POST /ringing/v2/commands`(统一命令),不再宣称 `/sessions/{id}/driver/*` URL;`qaqh-ringing` 增加规范 HTTP 路由表常量,daemon `build_router` 从常量表生成注册、exporter 校验 coverage 路由 ⊆ 常量表——双侧漂移闸。

## D8(B07)open handshake 契约代际

- `RingingV2OpenResponse` 增加 `command_contract_revision: String`(= `COMMAND_CONTRACT_REVISION`,`ringing-commands-6`);open handler 填充;`validate()` 要求非空。
- 兼容策略:旧客户端忽略未知字段(行为不变,命令阶段仍有 `unsupported_contract_revision` 明确拒绝);新客户端在发送命令前校验该字段并给出可读错误(SDK 跟进项,写入 decisions 供 SDK 侧执行)。

## D9(B08)8 个 service DTO

全部 36 个方法进入 typed ServiceMap,无 pending 项:

| 方法 | params DTO | result DTO |
| --- | --- | --- |
| config.load | `{}` | `ConfigDto`(已有 TS 导出) |
| diagnostics.memory.snapshot | `{ after_sequence? }` | `MemorySnapshot`(qaqh-memwatch,已有 TS) |
| plan.read | `{ session_id }` | `PlanItemView[]`(status 改规范枚举 `pending/approved/rejected/question`,替换旧 "✓/-/?" 字形;无 typed 消费方,现在收口) |
| profile.list | `{}` | `ProfileListSnapshot { profiles: BTreeMap<string, ProfileSummaryDto> }`——实现时发现 legacy profiles.toml 的持久化 schema 是 `qaqh_types::PersistentProfiles`(ProfileConfig),而 system.rs 旧实现用新 `ProfilesDocument` schema 读同一文件:任何有 profile 的真机 `profile.list`/`profile.create`/`profile.set_default` 都会 TOML 解析炸(B06 双 schema 共写缺陷,已修复)。摘要读模型与 ConfigDto.profiles_detail 同源 |
| session.dashboard | `{ session_id }` | `SessionDashboard { tasks: DashboardTask[], recent_edits: string[] }` |
| skills.list_tools | `{}` | `string[]` |
| todo.list | `{ session_id, status? }` | todo 信封形状的权威 DTO(实现时按 `to_envelope_value` 锁定) |
| todo.status | `{ session_id }` | `TodoStatusSnapshot`(实现时按 `todo_status_value` 锁定;无 todo 文件时 result 为 null) |

- 每个方法:TypedMethodInfo(params/result 校验)+ ServiceMap 项 + 代表性 fixture。
- coverage 幂等说明按真实 guard 修正(旧频道 / expected_revision / driver_epoch 文案清除)。

## 实施批次

1. 批次A(契约+session):events/state/types、owner(水位、declare 事件、rename 事件、publish)、view(title/subagents)、session_loop OwnerSink、qaqh-client 适配。
2. 批次B(receipt+daemon):pending_store sync_result、commands 同步结果落库、driver 席位注册表+dispatch、v2(open 代际、SSE resync、bootstrap overlay)、路由常量表。
3. 批次C(服务面+导出):8 个 DTO、handlers 类型化、exporter(类型/ServiceMap/fixtures/coverage/路由闸)。
4. 验收:`cargo check`/定向测试(-j2)→ exporter `check` → 重新生成 SDK generated + WebUI 镜像 → 隔离数据目录 daemon HTTP/SSE 冒烟。
