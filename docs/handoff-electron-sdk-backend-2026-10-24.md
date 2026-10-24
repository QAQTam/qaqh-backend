# 后端 → SDK/Electron 交接（2026-10-24）

- 对应规范：[`spec-electron-sdk.md`](spec-electron-sdk.md)，任务线 B02/B03 → F05。
- 交接状态：**可消费的部分契约切片；不是完整 Electron SDK 契约，也不是发布版 artifact。**
- 后端 revision：未锁定。本次交接来自当前共享工作树；不要把文档日期、`development` 或临时探针 revision 当成源 revision。
- 契约 owner：后端；SDK 必须从 exporter 产物导入，不能自行补齐 DTO、权限或 fallback 形状。

## 1. 当前可导出的内容

通过后端唯一入口生成：

```bash
cargo run -p qaqh-contract-export -- export --out <contract-dir>
cargo run -p qaqh-contract-export -- check --against <contract-dir>
```

产物布局遵循 SDK spec：`manifest.json`、`types/`、`index.ts`、`node.ts`、`constants.ts`、`service-map.ts`、`fixtures/`、`coverage.json`。当前 exporter 位于 `tools/qaqh-contract-export/`，只依赖契约相关 crate 与 build-time exporter；不会让 runtime/daemon 依赖 exporter。

### Ringing/wire 根类型

`@qaqh/sdk/protocol` 候选导入包括：

- open/lease：`RingingV2OpenRequest`、`RingingV2OpenResponse`、`RingingV2Capabilities`、`RingingV2LeaseRenewResponse`
- commands：`RingingCommand`、`RingingV2CommandEnvelope`、`RingingV2CommandAck`、`RingingV2CommandStatus`、`RingingV2ExistingResult`、`RingingV2CommandResult`
- event/reset：`RingingV2EventEnvelope<P>`、`RingingV2StreamKey`、`RingingV2Delivery`、`RingingV2ResetRequired`
- cursor/pending/driver/content 等基础类型

这些类型来自 `qaqh-ringing` 的 feature-gated ts-rs derive。`CanonicalCursor`/`CursorToken` 可被协议类型依赖；SDK UI 仍应把 cursor 当 opaque token 使用。

### B02 具体组合类型（2026-10-24 第二轮）

bootstrap/team/approvals/admin 与 content upload 的 handler 组合响应已从 `json!`/局部 struct 收口为一手 DTO，`index.ts` 提供具体组合的公共类型名：

```ts
export type ClientV2Bootstrap = RingingV2Bootstrap<ClientV2ControlState, ConversationSnapshot, ClientV2ToolState>;
export type ClientV2Event = RingingV2EventEnvelope<ProjectionPayload>;
export type ClientV2Payload = ProjectionPayload;
```

- 所有权：`qaqh-session/src/client_v2.rs` 定义 `ClientV2ControlState`/`ClientV2ToolState`/`ClientV2TeamResponse`（session→ringing 为合法向下依赖；qaqh-ringing 不反向依赖 session）。daemon bootstrap handler 与 `qaqh-client` 共用这一份定义，daemon 局部镜像已删除。
- 投影闭包：`ConversationSnapshot`/`TeamSnapshot`/`TaskBoardSnapshot`/`TaskView` 等投影类型补 feature-gated ts derive；事件 payload 用 `ProjectionPayload` 完整闭包（delta 词表随旧桌面导出线已有 derive）。TS 类型文件从 ~55 增至 ~196。
- approvals admin 面：`RingingV2PendingApprovals`/`RingingV2PendingPermissionView`/`RingingV2PendingInteractionView`（动态字段为 `unknown`/JSON value，键恒在）。device challenge 视图仍是另一种 opaque 形状，刻意不混用（coverage 标 pending）。
- content upload：`RingingV2ContentUploadResponse`；download 的 bytes/header 映射待 F 线联调（coverage 标 pending）。

Discovery 类型通过 **Node 专用** `node.ts` 导出，不在公共 `index.ts` 中导出：

```ts
export type { DaemonDiscovery } from "./types/qaqh/DaemonDiscovery";
```

`DaemonDiscovery.token` 是宿主凭据，只允许 Node/core credential adapter 使用；不得通过 Electron renderer bridge 暴露给 renderer。

### B03 typed service 当前切片

当前 descriptor 共 22 项：通用切片四项 + 账号面 18 项（siwc 8、dsaccount 10）。

| 方法 | Params | Result | kind | requires_session | admin_only |
| --- | --- | --- | --- | --- | --- |
| `daemon.version` | `DaemonVersionParams`（空对象） | `string` | read | false | false |
| `session.rename` | `SessionRenameParams { session_id: string; title: string }` | `null` | write | true | false |
| `fs.list` | `FsListParams { path: string; scope_session_id?: string }` | `FsEntry[]` | read | false | false |
| `fs.read` | `FsReadParams { path: string; max_bytes?: number; scope_session_id?: string }` | `FsReadResult` | read | false | false |
| `siwc.*`（8 项：status/profiles/models/sign_in.start/sign_in.status/sign_in.cancel/profile.select/sign_out） | 见 `qaqh_siwc::service` DTO | `SiwcStatusResult`/`SiwcProfilesResult`/`SiwcModel[]`/`SiwcSignInStartResult`/`SiwcSignInStatusResult`/`null`/`SiwcSignOutResult` | 按方法 | false | **true** |
| `dsaccount.*`（10 项：status/balance/bonuses/sign_in.start/sign_in.status/sign_in.cancel/sign_out/bonus.ack/models/verify） | 见 `qaqh_dsaccount::service` DTO | `DsAccount*` DTO/`null` | 按方法 | false | **true** |

`fs.*` 的 `scope_session_id` 是可选的文件系统范围收窄参数，不是 lease-bound `session_id`；`fs.read.max_bytes` 可省略，运行时默认 512 KiB 并限制最大读取 8 MiB。DTO 不承诺路径合法性：绝对路径及 allowed-roots 检查仍由既有 handler 执行。

账号面由 `qaqh-runtime/src/ringing/service_methods/accounts.rs` 单一收口：params 解码 → 调用管理器 → DTO 投影（drift 直接报错，不转发任意 JSON）。`QaqhService::handle` 的旧 siwc/dsaccount 分支已删除，service RPC 只有一条路径。账号 DTO 的所有权在 `qaqh-siwc`/`qaqh-dsaccount` 的 `service` 模块（camelCase 服务面、snake_case 轮询面、金额原串、nullable 字段必须显式在键）。`dsaccount.models` 经 `DsAccountModel { id, ..flatten }` 投影，缺 `id` 的条目会显式失败。`qaqh-runtime` 的 `ts` feature 现在转发到两个账号 crate。

Exporter 生成的 `service-map.ts` 形状（生成逻辑已通用化：只认 descriptor 类型名，`X`/`X[]`/`"string"`/`"null"`，不再手写方法名单）：

```ts
export interface ServiceMap {
  "daemon.version": { params: DaemonVersionParams; result: string; };
  "session.rename": { params: SessionRenameParams; result: null; };
  "fs.list": { params: FsListParams; result: FsEntry[]; };
  "fs.read": { params: FsReadParams; result: FsReadResult; };
  "siwc.status": { params: AccountsEmptyParams; result: SiwcStatusResult; };
  // … 账号面 18 项，见生成的 service-map.ts
}
```

运行时 dispatch 对 descriptor 命中的方法在执行副作用前 serde 校验 params、返回后校验 result。fs DTO/result 的负例与可选字段已加入 Rust 单测；账号面的参数负例、tagged 判别式负例、nullable-键存在性与"投影不含凭据"断言在 `service_methods/accounts.rs` 测试中。其余 service 方法尚未加入 typed descriptor，不能据此将它们视为已类型化 API。session/workspace/config/todo/git/plan/skills/diagnostics 等仍需后续 B03 工作。

注意：ts-rs 把空 struct（`DaemonVersionParams {}`、`AccountsEmptyParams {}`）输出为 `Record<symbol, never>`。已验证严格 TS 消费者可传 `{}`（`scripts/check-sdk-contract.py --typescript` 的 consumer 正例 + `@qaqh/sdk` typecheck/test 均通过）；该形状可用，不调整映射。

## 2. Fixtures 与 coverage

当前合成 fixture 由真实 Rust DTO 序列化并 serde 往返校验生成，含：

- `open-request.json`、`open-response.json`
- `command-attach.json`、`command-ack-accepted.json`、`command-status-accepted.json`
- `service-daemon-version-params.json`、`service-daemon-version-result.json`
- `service-session-rename-params.json`、`service-session-rename-result.json`
- `service-fs-list-params.json`、`service-fs-list-result.json`
- `service-fs-read-params.json`、`service-fs-read-result.json`
- 账号面 18 项的 `service-{siwc,dsaccount}-*-params/result.json`（由真实账号 DTO 构造，含 `0E-16` 金额原串、kebab-case 流状态、flatten 的模型扩展键）
- `bootstrap-response.json`（真实 `ClientV2Bootstrap` 组合，含 pending interaction 与 driver 席位，过 `validate()`）
- `team-response.json`（roster/inbox/task board/message board 三份快照）
- `approvals-pending.json` + `approvals-empty.json`（admin 面双槽位：空槽键在值为 null）
- `content-upload-response.json`、`event-envelope-team-delta.json`（ephemeral 事件不携带可靠位置字段——该约束由 envelope `validate()` 强制）

无真实 token、真实用户路径、账号、会话正文（fs fixture 使用 synthetic 路径）。Exporter 产物的 `coverage.json` 当前把以上 22 个 service 能力 + `session.bootstrap-watch`/`session.team`/`approvals.admin`/`content.upload` 标为 implemented；`timeline.snapshot-watch`/`approvals.device`/`content.download`/`daemon.control` 与其余 service methods 为 pending（各带 reason）。

这些是 DTO/wire 往返样本，不是 endpoint 级测试，亦未证明真实 handler/error/权限/lease/driver 行为。

## 3. 生成、校验与版本门禁

本地开发 artifact：

```bash
cargo run -p qaqh-contract-export -- export --out /tmp/qaqh-sdk-contract
cargo run -p qaqh-contract-export -- check --against /tmp/qaqh-sdk-contract
```

正式 artifact 必须由发布编排注入真实 metadata，且工作树干净：

```bash
QAQH_SOURCE_REVISION=<actual-revision> QAQH_SOURCE_DIRTY=0 \
  cargo run -p qaqh-contract-export -- export --out <contract-dir> --release
QAQH_SOURCE_REVISION=<actual-revision> QAQH_SOURCE_DIRTY=0 \
  cargo run -p qaqh-contract-export -- check --against <contract-dir> --release
```

`--release` 缺少 revision 或 clean 标记会失败。Manifest 含文件 SHA-256、contract hash、wire/backend/generator version、revision/dirty 状态。契约 hash 是产物完整性标记，不应作为运行时连接必须相等的条件。

Exporter 对已有目标只允许更新本工具版本 manifest 所有的目录，并拒绝 symlink/非 exporter 目录；生成在独立 staging，失败不应交付半套目录。

## 4. 已执行验证

本轮（2026-10-24 B02 具体组合切片）确认通过：

- `cargo check --workspace --all-targets`；`cargo check -p qaqh-session --features ts`
- `cargo test -p qaqh-session --lib`（67 项，含 client_v2 wire 形状锁）、`cargo test -p qaqh-client --lib`（71 项）、`cargo test -p qaqh-daemon -- --test-threads=1`（90 项，bootstrap/team/approvals/content handler 已走 DTO）、`cargo test -p qaqh-ringing --lib`（19 项）、`cargo test -p qaqh-runtime --lib ringing::`（100 项）
- `cargo test -p qaqh-contract-export`（6 项）
- `python3 scripts/check-sdk-contract.py --typescript`（消费者正/负例扩到 bootstrap 组合、approvals 必备键、content upload）
- 产物已导入 `../qaqh-sdk/src/protocol/generated`（TS 类型文件 ~196 个）并验证 `check --against` 一致、`pnpm run typecheck`/`test`/`build` 全过（SDK 的 coverage 守卫测试已更新到新基线：bootstrap-watch/team/approvals.admin/content.upload implemented，timeline/device/download 显式 pending）

上一轮（同日账号切片）记录：runtime lib 324 项全过；export→check 往返一致；ServiceMap 22 项。期间 CI 曾因 exporter 引用的 `ReceiptPolicy` 所在 pending_store 改动漏提交而红，已补提交修复。

既有平台性失败（与本轮改动无关）：`qaqh-siwc` 2 项、`qaqh-dsaccount` 3 项 at-rest 加密断言在 Linux 必然失败——`qaqh-config::secrets` 非 Windows 平台 `encrypt()` 为恒等（明文 + 0600，TODO keyring），而这些测试断言磁盘无明文 token。修复需 siwc/dsaccount owner 裁决（keyring 集成或 `cfg(windows)` 门控测试），不在本轮擅自处理。

workspace 中其他 crate 会产生既有 warning（process-tools unused variable、workspace unused mut、client dead code）；本交接不代表全 workspace clippy 或真实 Electron/daemon 联调已通过。

### 本轮新增的相邻修复

- `RingingV2OpenRequest` 补 `validate()`（镜像 daemon open handler 的 schema/version 拒绝 + 非空 client_instance_id）；此前只有 `RingingV2OpenResponse` 有。exporter 测试的负例依赖它。

## 5. 未交付 / F 线不可假定

- timeline 快照/分页/watch 的 DTO 接线、负例与 handler fixtures 仍未完成（`TimelinePage` 尚在 `qaqh-client` 且无 ts 导出）。
- device 审批 challenge 视图（opaque 形状）与 content 下载的 bytes/header 契约未收口；远端/TLS 验收延后。
- 除 `daemon.version`/`session.rename`/`fs.*` 与账号面 18 项外的 `METHODS`（session/workspace/config/profile/todo/plan/git/skills/diagnostics/subagent）暂无 typed `ServiceMap` 声明；禁止用 `any`/调用者自选泛型伪装已覆盖。
- SDK 的 generated 导入 + pnpm TypeScript 编译/测试已验证；tarball 独立安装、Electron IPC 消费或 daemon/Electron 真实集成测试尚未完成。
- pairing/device DTO 虽可导出，远端/TLS 验收仍不在当前本地 Electron 发布门槛内。

收到契约漂移、DTO 不可用或 TS 空对象映射问题时，请附失败 fixture/编译错误回 B 线；SDK 不增加多形状 fallback。

## 6. 下一步

1. F 线：bootstrap/watch/SSE 的真实类型已就位（`ClientV2Bootstrap`/`ClientV2Event`/`ClientV2Payload` 及完整投影闭包），F03/F04 可从 mock 切换到真实生成物；SSE decoder/恢复语义按 spec §5.3 实施。
2. B 线继续 B03：剩余方法（session.list/meta/activity/dashboard/get_activity/set_profile/set_tool_mode、workspace.*、config.*/profile.*、todo.*/plan.read、git.*、skills.*、diagnostics.memory.*、subagent.spawn）逐方法接 DTO；优先有 Electron 实际调用入口的方法。
3. B 线收尾 B02：timeline DTO/fixtures（`TimelinePage` 提取到契约层）；device challenge 与 content download 契约随 F 线联调定稿。
4. 裁决 siwc/dsaccount 的 Linux at-rest 测试（见 §4）：keyring 集成或平台门控。
5. 新 descriptor 加入时，同步更新 Rust descriptor/runtime 双向测试、exporter ServiceMap 生成与 fixtures；ServiceMap 生成已通用化，只需在 `TYPED_METHODS`/accounts `TYPED_METHODS` 加条目并补 fixture。
