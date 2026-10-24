# Electron SDK 后端覆盖矩阵（B01 初稿）

- 对应规范：[`spec-electron-sdk.md`](spec-electron-sdk.md)，任务 B01。
- 状态：初稿；静态源码盘点，**不是** exporter、fixture、行为测试或完整覆盖声明。
- 盘点基线：当前工作树；尚未记录/锁定 revision，不能作为发布契约产物。
- 原则：只登记当前代码可见入口。SDK 能力目标不自动意味着后端已经提供该能力。

## 入口、授权与 DTO 现状

路由真源为 `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:161-222`。除 `/health` 和 `/ringing/v2/pair` 外，认证中间件要求 Bearer 身份（`authz.rs:80-98`）。身份有本地 Admin 与设备身份；设备 scope 为 `view < interact < admin`（`authz.rs:12-32`）。逐 endpoint/command 的下表档位是已确认的最低要求；admin 恒可通过 scope 检查。

| 能力 ID | 当前入口/方法 | params / result DTO | 授权/归属 | 证据与状态 |
| --- | --- | --- | --- | --- |
| discovery.local | daemon discovery 文件 `daemon.json`（非 HTTP） | `qaqh_types::DaemonDiscovery`，含 endpoint/token/pid/server_epoch/protocol_version/daemon_version 及可选扩展字段 | 宿主本地文件；token 仅宿主消费，绝不暴露 renderer | `crates/qaqh-client/src/discovery.rs:10,51-59`; SDK node 适配职责，DTO 来源须 B02 导出 |
| connect.open | `POST /ringing/v2/clients/open` | `RingingV2OpenRequest` → `RingingV2OpenResponse` | Bearer；Admin 或有效 device | `qaqh-daemon/.../mod.rs:165`; `qaqh-daemon/.../v2.rs:78-132`; `qaqh-ringing/src/v2/types.rs:17-22,59-69`; implemented static |
| connect.renew | `POST /ringing/v2/leases/renew` | 无 body → `RingingV2LeaseRenewResponse` | Bearer + client-session header | `mod.rs:166`; `v2.rs:134-158`; `types.rs:146-151`; implemented static |
| session.bootstrap | `GET /ringing/v2/sessions/{session_id}/bootstrap` | `ClientV2Bootstrap`（generic wire bootstrap 绑定 control/conversation/tool） | lease 绑定该 session | `mod.rs:168-170`; `v2.rs:160-281`; `qaqh-client/src/v2.rs:152-183`; implemented static，响应组合类型在 daemon handler 尚为局部 struct |
| session.watch | `GET /ringing/v2/sessions/{session_id}/events?since_cursor=` | SSE 事件封套 `RingingV2EventEnvelope<ProjectionPayload>` / reset | lease 绑定该 session；流按 session 单条 | `mod.rs:175-178`; 生成具体类型候选 `qaqh-client/src/v2.rs:37-50,146-150`; implemented static，需 B02 确认公开闭包/fixture |
| session.team | `GET /ringing/v2/sessions/{session_id}/team` | `ClientV2TeamResponse`；handler 目前由 JSON 组合 schema/session_id/team/tasks/board | lease 绑定该 session | `mod.rs:167-174`; `v2.rs:284-332`; `qaqh-client/src/v2.rs:131-144`; implemented static，daemon 响应用 `json!` |
| command.submit | `POST /ringing/v2/commands/{id}`（id 为 channel） | `RingingV2CommandEnvelope` → `RingingV2CommandAck` | attach 为 View；其他命令 Interact；lease/session ownership 与命令校验 | `mod.rs:180-182`; `command.rs:56-90,129-160`; B02 类型导出待办 |
| command.status | `GET /ringing/v2/commands/{command_id}` | `RingingV2CommandStatus` | Bearer + 发起 client session 的 lease | `mod.rs:180-182`; `qaqh-client/src/v2.rs:472-491`; implemented static；未知/过期 receipt 语义需 B04 fixture/测试确认 |
| driver.claim/release | `POST .../driver/claim`, `POST .../driver/release` | `RingingV2DriverClaimResponse` / `RingingV2DriverReleaseResponse` | Interact + session lease；driver epoch guard | `mod.rs:184-190`; `command.rs:119-137`; typed response 已在 `qaqh-ringing/src/v2/types.rs` 导出路径中，B02 检查 |
| approvals.admin | `GET .../sessions/{session_id}/approvals` + canonical command channel 回答 | 当前 admin pending 形状由 `json!` 产生；ask/plan/permission 命令属于不同 command variant | lease；Admin desktop 路径 | `mod.rs:192-198`; `v2.rs:334-428`; domain variants `qaqh-domain/src/command.rs:112-147,321-328`; pending DTO 待提取 |
| approvals.device | `POST .../sessions/{session_id}/approvals/respond` | handler 私有 `ApprovalRespondRequest`（challenge_id/decision/payload）→ command ACK | Interact + session lease；一次性 challenge；不要求 driver | `mod.rs:195-198`; `v2.rs:430-509`; 明确不可与 admin canonical approval 形状混用 |
| timeline.snapshot | `GET .../sessions/{session_id}/timeline?before_index=&limit=` | `TimelinePage` 候选；handler JSON 含 snapshot/has_more/total_turns/truncated_before | session lease | `mod.rs:199-202`; `timeline_api.rs:70-174`; `qaqh-client/src/types.rs:81-115`; DTO 基本存在，JSON 输出仍需真实 fixture 校验 |
| timeline.watch | `GET .../sessions/{session_id}/timeline/events` | Timeline SSE frame / status | session lease | `mod.rs:207-209`; route 确认；B02 需闭包/fixture 校验 |
| content.upload | `POST /ringing/v2/content` multipart(session_id,media_type,content) → JSON(content_id,media_type,sha256,size,truncated) | result 目前无显式 DTO | Interact + 上传 session 必须归属 lease | `mod.rs:203-204`; `content.rs:194-297`; 待 B02 提取实际 response DTO |
| content.download | `GET /ringing/v2/content/{content_id}` bytes + Content-Type/Range headers | 二进制响应；支持单 Range | 有效 lease；必须拥有引用内容的 session；未拥有时不泄露存在性 | `mod.rs:203-204`; `content.rs:7-52,99-133`; 明确需定义 SDK bytes/header 映射 |
| service.rpc | `POST /ringing/v2/service/{method}` | 当前统一 `serde_json::Value` params/result；没有每方法 DTO 描述符 | `METHODS` 声明 read/write、requires_session、admin_only；session 参数检查见表实现 | `mod.rs:205`; `service_methods.rs:15-31,84-179`; **B03 blocker**：不能以泛型调用当 typed contract |
| pairing.issue / pair | `POST /ringing/v2/pairing/tokens`; `POST /ringing/v2/pair` | 已有 `RingingV2PairTokenRequest/Response`, `RingingV2PairRequest/Response` | 签发 Admin；pair endpoint 免 Bearer、一次性 pairing token | `mod.rs:211-214`; `pairing.rs:116-193`; 按 SDK spec 延后发布门槛但 DTO 必须 B02 导出 |
| devices.list / revoke | `GET /ringing/v2/devices`; `POST .../devices/{device_id}/revoke` | `RingingV2DevicesResponse`; revoke 204 | Admin | `mod.rs:215-218`; `pairing.rs:195-245`; spec 标注延后，但不能与 admin SDK 已覆盖混淆 |
| daemon.health/activity/stop | `GET /health`, `GET /activity`, `POST /control/v1/stop`, `POST /control/v1/stop-if-idle` | 独立 control response；需进一步核对稳定 DTO | health 免鉴权；其他路由 Bearer（档位需逐 handler 确认） | `mod.rs:163-164,220-221`; 尚不纳入首批 typed 契约，B01 待补 authz/error/detail |

`...` 在上表仅表示路径的重复固定前缀 `/ringing/v2`，实际 endpoint 以 `mod.rs` 为准。

## Service 方法覆盖初稿

服务表位于 `crates/qaqh-runtime/src/ringing/service_methods.rs:92-171`。当前 `MethodInfo` 只提供 `kind: Read|Write`、`requires_session`、`admin_only`（23-31）；不能推导准确 params/result，更没有显式额外 scope 参数模型。下列按代码方法表原样归组，所有行的逐方法 DTO 均为 **pending B03**。

| 组 | 已登记方法 | 元数据要点 | 状态 |
| --- | --- | --- | --- |
| daemon/session | `daemon.version`, `session.list`, `session.meta`, `session.activity`, `session.dashboard`, `session.get_activity`, `session.set_profile`, `session.rename`, `session.set_tool_mode` | meta/dashboard/get_activity 与 set_profile/rename/set_tool_mode 需要 session；写项分类见 `METHODS` | `daemon.version` 与 `session.rename` 已有 B03 typed descriptor/边界验证；其余 pending DTO |
| workspace | `workspace.get`, `workspace.list`, `workspace.set`, `workspace.create`, `workspace.rename`, `workspace.delete`, `workspace.move_session`, `workspace.detach` | get 需要 session；set/move_session/detach 声明需要 session；其余写操作无该布尔标记 | pending DTO；需核对 params 中会话键归属语义 |
| filesystem | `fs.list`, `fs.read` | Read, `requires_session=false`; DTO 包含可选 `scope_session_id`（收窄 allowed roots，不是 lease session），`fs.read.max_bytes` 可选 | B03 typed DTO/ServiceMap/serde fixtures 已接入；真实 handler/权限行为仍由既有测试覆盖，不能仅凭 DTO 宣称 endpoint contract 完整 |
| config/profile | `config.load`, `config.save`, `config.set_permission_level`, `profile.apply`, `profile.save_current`, `profile.delete` | 普通 Read/Write | pending DTO |
| accounts | `siwc.status`, `siwc.profiles`, `siwc.models`, `siwc.sign_in.start`, `siwc.sign_in.status`, `siwc.sign_in.cancel`, `siwc.profile.select`, `siwc.sign_out`; `dsaccount.status`, `dsaccount.balance`, `dsaccount.bonuses`, `dsaccount.sign_in.start`, `dsaccount.sign_in.status`, `dsaccount.sign_in.cancel`, `dsaccount.sign_out`, `dsaccount.bonus.ack`, `dsaccount.models`, `dsaccount.verify` | 全部 `admin_only=true`；siwc/dsaccount 各自 `methods::ALL` 双向覆盖测试已存在 | B03 typed descriptor 已接入：DTO 所有权在 `qaqh-siwc`/`qaqh-dsaccount` 的 `service` 模块，runtime 侧由 `ringing/service_methods/accounts.rs` 逐方法 params 解码→调用→DTO 投影（drift 报错）；ServiceMap/fixtures/coverage 同步生成；admin 约束经 descriptor↔方法表双向测试保留 |
| skills | `skills.list_tools`, `skills.operation`, `skills.reload` | Read/Write；生命周期类 skills.activate 不在 service 表 | pending DTO |
| todo/plan | `todo.status`, `todo.list`, `todo.set`, `plan.read` | todo.* 需要 session；todo.set Write | pending DTO |
| diagnostics | `diagnostics.memory.snapshot`, `diagnostics.memory.start`, `diagnostics.memory.stop` | 全部 admin-only | pending DTO；SDK admin 子面单独放行 |
| git | `git.diff`, `git.branch`, `git.branches`, `git.file_diff`, `git.switch_branch`, `git.commit` | 查询项 requires_session；变更项不要求 `requires_session` | pending DTO；确认参数归属/副作用语义 |
| subagent | `subagent.spawn` | Write | pending DTO；不要据此替代现有 control/conversation 命令语义 |

会话生命周期方法不得从 service 表补入；命令定义在 `crates/qaqh-domain/src/command.rs:46-170,243-301,307-329`。已确认命令按频道包括 control（create/resume/attach/close/archive/unarchive/delete/shutdown/config reload/tool mode/ask/plan/driver/skills/resource changed）、conversation（send/cancel/undo/compact/load more/set mode）、tool（invoke/permission respond）。B02 必须导出真实 command 及传递 DTO 闭包，不应在 SDK 复制。

## 命令幂等范围初步观察（需 B04 验证）

- receipt store 将命令映射存于 `crates/qaqh-runtime/src/ringing/pending_store.rs`，具有 300 秒 `RECEIPT_TTL`（552），并可在初始化时读取单独 receipt 文件；现有状态不是 canonical facts 权威。
- `record_fingerprint_for_session` 将 command ID、payload fingerprint 和 client-session lease 绑定；相同 ID 不同指纹/lease 不应视为相同重放（`pending_store.rs:196-242`）。daemon fingerprint 包含 channel、session、expected_revision、driver_epoch 和 command（`axum_impl/command.rs:35-54`）。
- status 查询及重放 receipt 都受 owning client session 约束（`pending_store.rs:468-518`）。因此“原 ID 查询/重放”当前有 lease/client-session 作用域和 TTL；不能承诺跨新 open lease 可查询或安全重放。
- `PendingCommandStore::open`/落盘恢复需进一步核验状态、崩溃时序与不同 daemon epoch 行为。仅源码静态阅读不足以证明 ACK 丢失、重启、receipt 缺失时的 exactly-once 或安全重试；这些列为 B04 pending。SDK 在 B04 证据到齐前只能保留原 ID 查询，并将无法确认的 outcome 报为 unknown，不自动重发副作用命令。

## B01 发现记录 / 待裁决

- `spec-electron-sdk.md:32,165` 规定本轮不新增深历史分页 API，但当前 `timeline_api.rs:10-11,115-135` 已实现基于 `archive_turn_page` 的分页；`TimelinePage.truncated_before` 描述物化窗口范围（`qaqh-client/src/types.rs:103-114`）。这不构成冲突：SDK 可消费当前现有 endpoint/行为，本轮不得另造路由或分页语义。覆盖矩阵以当前 `before_index`/`limit` 和 `truncated_before` 为准；具体是否由 Electron UI 深翻页属于客户端范围裁剪。
- `spec-electron-sdk.md:5` 指明工作仓包括新建同级 `qaqh-sdk`、`qaqh-electron`，而 B 线不改 Electron 壳；本盘点只覆盖 backend 仓边界。

## B02 类型/生成器差距盘点（实现前置）

### 现有生成基础

- `justfile:34-39` 说明既有桌面仓通过 `TS_RS_EXPORT_DIR` 汇集 `qaqh-types/domain/ringing/session/config-api` 的 derive；该命令已随桌面仓拆分，本 backend 当前没有统一导出 recipe 或 exporter crate。
- `qaqh-domain`、`qaqh-ringing`、`qaqh-session`、`qaqh-types` 的 `ts` feature 均为 optional derive feature（各 crate `Cargo.toml`）；这是可复用的生成基础，但仅代表带注解类型，不代表传递闭包齐全。
- `qaqh-ringing/src/command.rs` 的 `RingingCommand` 已有 `cfg_attr(feature="ts", derive(TS))`；domain commands 和部分 projection/event/types 也已有注解。导出物目前统一写到旧 namespace `qaqh/`，后端 exporter 应以隔离的 export root 收集，再生成自己的 `index.ts` 与 metadata，不应直接复用/覆盖桌面仓目录。

### 首批阻塞缺口

- 已在本轮工作树为 `qaqh-ringing/src/v2/types.rs` 的首批公开 v2 DTO 增加 feature-gated TS derive；仍需以隔离的 TS_RS_EXPORT_DIR 验证实际生成内容及完整 import 闭包。`v2/mod.rs` 的 Rust `pub use` 本身不会自动注册或导出 TS 类型。
- 具体 bootstrap 绑定类型目前在 `qaqh-client/src/v2.rs`：`ClientV2ControlState`、`ClientV2ToolState`、`ClientV2Bootstrap` 与 `ClientV2Event`。该 crate 没有 `ts` feature，且 `qaqh-ringing` 不能依赖 `qaqh-session`；导出具体组合时应选正确层级新增/重用 public contract DTO，而不是让 wire crate 反向依赖 projection/session。
- `RingingV2CommandAck.existing` 和 `RingingV2CommandStatus.result` 含终态 payload；源码 derive 已覆盖 `RingingV2ExistingResult` / `RingingV2CommandResult`，仍须在隔离生成流程中校验所有 import 闭包。
- `DaemonDiscovery` 位于 `qaqh-types/src/discovery.rs`，当前已添加 TS derive；该类型带宿主 token，导出物必须标明仅 `@qaqh/sdk/node` 使用，不得由公共 renderer bridge 暴露。
- TimelinePage、approval handler 私有 request/result、team `json!` 返回值、content upload JSON 均未形成完整的一手生成闭包。导出前要用实际 handler 采用 DTO，确保 JSON 形状不变；不能 exporter 单独造镜像。当前 fixtures 仅覆盖 open request/response、attach command、accepted ACK/status 的基础 wire DTO 往返，不覆盖这些 handler 组合响应。

### 建议 B02 按依赖闭包分阶段验收

1. 先在真实 wire/domain 类型上补 TS derive 和 `ts` feature 闭包，集中固定 ts-rs 数字策略；首批包括 `DaemonDiscovery`（Node 内部面）、open/renew、command envelope/ack/status、generic event/bootstrap 根类型及 payload 的实际 projection 类型。
2. 为 handler 当前的私有/JSON 组合响应确定契约所有者，并让 handler 直接序列化这些 DTO；先做 bootstrap、team、timeline、approval pending、content upload，再按实际 SDK 能力覆盖扩展。approval 的 device challenge DTO 与 admin pending DTO 必须分开。
3. 再加独立 `qaqh-contract-export`：只协调 feature-gated type export 与产物 staging/manifest，不引入 runtime/daemon 依赖；用固定测试 fixture 从真实 DTO serialize/deserialize，且确保输出目录有唯一权威版本。
4. `export` 使用 sibling staging 临时目录；只有全部类型生成、fixture 验证、manifest/hash 校验成功后才发布目标目录。失败保留旧目标不动。`check` 生成到临时目录并比较完整文件集合，缺失/陈旧/额外文件均失败。
5. exporter 在 B02 首批只将尚未实现的 service params/results 标为 pending；禁止生成泛 `Record<string, any>`/`any` 的虚假 ServiceMap。B03 接入 descriptors 后再填 service-map。

### 实施进度（当前工作树）

- 已为 `qaqh-ringing/src/v2/types.rs` 的公开 v2 wire DTO 添加 feature-gated `TS` derive，包含 open/capabilities/renew、stream key/event/reset、pending interaction/content value、driver、generic bootstrap/channel snapshot、command envelope/ack/existing/result/status。
- 已为 `qaqh-ringing/src/v2/cursor.rs` 的 `CanonicalCursor`、opaque `CursorToken` 和 `RingingV2InteractionKind` 补齐 derive 依赖闭包。`qaqh-types/src/discovery.rs` 的 `DaemonDiscovery` 也已加 feature-gated TS derive。
- 新增 `tools/qaqh-contract-export` workspace 工具，按隔离 staging 生成固定布局 `types/`, `index.ts`, `node.ts`, `constants.ts`, `service-map.ts`, `fixtures/`, `coverage.json`, `manifest.json`；ts-rs `Config` 固定 `u64/i64 → number`、不读取 `TS_RS_*` 环境变量。Discovery token 类型只从 `node.ts` 导出，不进入公共 `index.ts`。
- `fixtures/` 已包含由正式 Rust DTO 序列化并往返验证的合成 open request/response、attach command、accepted ACK/status，以及 `daemon.version` / `session.rename` 的 service params/result JSON；coverage 按能力分别列明已实现切片与仍 pending 项。
- B03 首个 typed service 切片已接入 `daemon.version`、`session.rename`：`TYPED_METHODS` 为这两项绑定 params/result DTO、Read/Write、requires_session/admin_only 和 serde 验证函数；runtime dispatcher 在执行前校验 params、返回后校验结果。Exporter 从同一 descriptor 生成两项 ServiceMap/metadata，不再为这两项手写另一份权限名单。
- B03 账号面切片（2026-10-24 第二轮）已接入 siwc 8 项 + dsaccount 10 项：`ringing/service_methods/accounts.rs` 持有账号 descriptor 与逐方法 typed dispatch；`QaqhService::handle` 的旧 siwc/dsaccount 分支已删除（单一权威）；`qaqh-runtime` 的 `ts` feature 转发到两个账号 crate；`RingingV2OpenRequest` 补 `validate()`；exporter 的 ServiceMap 生成通用化（descriptor 驱动，`X`/`X[]`/primitives），账号 fixtures（params/result 36 份）与 coverage 合并两份清单；`scripts/check-sdk-contract.py --typescript` 消费者正/负例扩到账号方法。产物已导入 `../qaqh-sdk` 并通过 typecheck/test/build。
- B02 具体组合切片（2026-10-24 第三轮）：`qaqh-session/src/client_v2.rs` 成为 bootstrap/team 具体组合的唯一权威（`ClientV2ControlState`/`ClientV2ToolState`/`ClientV2TeamResponse`；session→ringing 合法向下依赖）；daemon bootstrap 局部镜像删除、team 改 struct 序列化；approvals admin 面（`RingingV2PendingApprovals` 等 3 DTO）与 content upload（`RingingV2ContentUploadResponse`）从 `json!` 收口；投影类型（conversation/team/task board 闭包）补 ts derive，TS 类型文件 ~55 → ~196；`index.ts` 生成具体组合别名 `ClientV2Bootstrap`/`ClientV2Event`/`ClientV2Payload`；fixtures 增 bootstrap/team/approvals×2/content-upload/event-envelope；coverage 将 bootstrap-watch/team/approvals.admin/content.upload 翻 implemented，timeline/approvals.device/content.download 显式 pending 带 reason。期间 CI 因 pending_store 收据元数据漏提交而红过一次，已补提交修复。
- Manifest 包含规范要求字段、排序文件哈希和 contract hash。无参数时产物标 `source_revision=development`、`source_dirty=true`。`--release` 必须有 `QAQH_SOURCE_REVISION` 且 `QAQH_SOURCE_DIRTY=0|false`，否则拒绝。
- `export` 在 sibling staging 生成/校验后提交；只允许更新带有本 exporter 版本 manifest 的既有目录，拒绝 symlink/非 exporter 目标；更新时先备份再 rename，提交失败恢复原目录。`check` 临时重生成并比较完整文件集合和字节；新增单测覆盖文件内容差异与额外文件。
- 已验证通过：`cargo check -p qaqh-ringing --features ts`；`cargo check -p qaqh-types --features ts`；`cargo check -p qaqh-ringing`；`cargo fmt --manifest-path tools/qaqh-contract-export/Cargo.toml -- --check`；`cargo test -p qaqh-contract-export`；开发模式 export→check；release 缺失 revision 被拒；注入测试 revision + clean 标记后 release export→check 通过。实际生成物只在 `/tmp` 探针目录，不提交到源码树。
- 全 workspace `cargo fmt --all -- --check` 曾报告 `../mutil-ai` 与其他 workspace 文件既有格式差异；没有批量格式化这些无关文件。
- **B02/B03 仍未完整**：`coverage.json` 逐能力标注当前 wire 根覆盖与 pending；bootstrap/team/approvals.admin/content.upload 的 handler DTO 已接线并有 fixtures，timeline 快照/分页/watch、device challenge 视图与 content download 的 bytes 契约仍未收口。`service-map.ts` 现由 B03 descriptor 生成 22 项（`daemon.version`、`session.rename`、`fs.*` 2 项、siwc 8 项、dsaccount 10 项）；其余已登记 service 方法（session/workspace/config/profile/todo/plan/git/skills/diagnostics/subagent）仍 pending typed descriptor/DTO。当前导出物不可作为“完整 Electron SDK”契约发布。
- 既有平台性测试失败（非本轮引入）：`qaqh-siwc` 2 项、`qaqh-dsaccount` 3 项 at-rest 加密断言在 Linux 必然失败——`qaqh-config::secrets` 非 Windows 平台为明文 + 0600（TODO keyring）。需 owner 裁决 keyring 集成或测试平台门控。

Exporter 的命令、release 元信息变量已同步到 `spec-electron-sdk.md:86-94`。

## B01 结论与 B02/B03 交接

1. 首批闭环的路由和主要类型入口存在：open → attach（`ControlCommand::SessionAttach`）→ bootstrap/watch → send command → status → disconnect（disconnect 是客户端释放 lease/timer，不存在 daemon disconnect 路由）。attach 与 resume 是不同命令。
2. `qaqh-ringing` 已有 v2 基础 wire DTO，`qaqh-client` 已组合具体 projection DTO；但 handler 中仍有局部 bootstrap state、`json!` 的 team/approval/content/timeline 响应，必须在使用点提取/迁入正确契约模块后由实际 handler 序列化，不能创建未接线镜像。
3. service 表有方法名和粗粒度权限，但没有 per-method typed params/result，fs scope 也未由 `MethodInfo` 表达。B03 是发布 typed service API 的硬依赖。
4. pairing/devices DTO 可导出；功能验收依 `spec-electron-sdk.md:34,163` 明确延后，不能宣称远端/TLS 已验收。
5. 下一步 B02 应交付规范要求的固定布局 `manifest.json`, `types/`, `index.ts`, `constants.ts`, `service-map.ts`, `fixtures/`, `coverage.json`；首批 coverage 可将完整 service 行列为 pending，但要保证 connect/attach/bootstrap/watch/send/status 闭环真实有 DTO 与 fixtures。B03 在实际 service handler/request/response 接线完成前不能生成可信 `ServiceMap`。
