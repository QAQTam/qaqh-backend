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

无真实 token、真实用户路径、账号、会话正文（fs fixture 使用 synthetic 路径）。Exporter 产物的 `coverage.json` 当前把以上 22 个 service 能力标为 implemented；`session.bootstrap-watch`、team/approvals/content/timeline handler 组合响应与其余 service methods 为 pending。

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

本轮（2026-10-24 账号切片）确认通过：

- `cargo check -p qaqh-runtime --features ts`、`cargo check -p qaqh-daemon`、`cargo check --workspace --all-targets`
- `cargo test -p qaqh-runtime --lib -- --test-threads=1`：324 项全过（含 service_methods 23 项：合并两份 TYPED_METHODS 的 descriptor↔方法表双向测试 + accounts 参数/判别式/nullable/无凭据断言）
- `cargo test -p qaqh-contract-export`（6 项）；`cargo fmt --check`（runtime/ringing/siwc/dsaccount/exporter）
- `cargo run -p qaqh-contract-export -- export --out /tmp/qaqh-contract-accounts` + `check --against` 往返一致
- `python3 scripts/check-sdk-contract.py --typescript`（严格 tsc 独立消费者：ServiceMap 正/负例已扩到账号方法——缺 `flow_id`、非法 `status` 枚举均为编译错误；`admin_only: true` 元数据可推导）
- 产物已导入 `../qaqh-sdk/src/protocol/generated` 并在 SDK 仓验证：`pnpm run typecheck`、`pnpm test`（2 项）、`pnpm run build` 全过

另有既有平台性失败（与本轮改动无关，siwc/dsaccount 依赖图不含本轮改动的任何 crate）：`qaqh-siwc` 2 项、`qaqh-dsaccount` 3 项 at-rest 加密断言在 Linux 必然失败——`qaqh-config::secrets` 非 Windows 平台 `encrypt()` 为恒等（明文 + 0600，TODO keyring），而这些测试断言磁盘无明文 token。修复需 siwc/dsaccount owner 裁决（keyring 集成或 `cfg(windows)` 门控测试），不在本轮擅自处理。

workspace 中其他 crate 会产生既有 warning（process-tools unused variable、workspace unused mut、client dead code）；本交接不代表全 workspace clippy 或真实 Electron/daemon 联调已通过。

### 本轮新增的相邻修复

- `RingingV2OpenRequest` 补 `validate()`（镜像 daemon open handler 的 schema/version 拒绝 + 非空 client_instance_id）；此前只有 `RingingV2OpenResponse` 有。exporter 测试的负例依赖它。

## 5. 未交付 / F 线不可假定

- 具体 bootstrap projection/event 的 `ControlState`、conversation/tool state 组合类型尚未完成对外契约闭包；当前通用 `RingingV2Bootstrap<C,V,T>` 不能直接当成产品具体类型。
- timeline、team、admin/device approvals、content upload、handler `json!` 组合响应尚未全部改为实际使用的一手公开 DTO，fixtures 也未覆盖这些 handler。
- 除 `daemon.version`/`session.rename`/`fs.*` 与账号面 18 项外的 `METHODS`（session/workspace/config/profile/todo/plan/git/skills/diagnostics/subagent）暂无 typed `ServiceMap` 声明；禁止用 `any`/调用者自选泛型伪装已覆盖。
- SDK 的 generated 导入 + pnpm TypeScript 编译/测试已验证；tarball 独立安装、Electron IPC 消费或 daemon/Electron 真实集成测试尚未完成。
- pairing/device DTO 虽可导出，远端/TLS 验收仍不在当前本地 Electron 发布门槛内。

收到契约漂移、DTO 不可用或 TS 空对象映射问题时，请附失败 fixture/编译错误回 B 线；SDK 不增加多形状 fallback。

## 6. 下一步

1. F 线：账号面 18 个方法已在 `ServiceMap` 中可用（`client.service("siwc.status", {})` 形态自动获得类型推导）；继续 bootstrap/watch/SSE/timeline 等 F03/F04 切片，B02 到位前用 mock。
2. B 线继续 B03：剩余方法（session.list/meta/activity/dashboard/get_activity/set_profile/set_tool_mode、workspace.*、config.*/profile.*、todo.*/plan.read、git.*、skills.*、diagnostics.memory.*、subagent.spawn）逐方法接 DTO；优先有 Electron 实际调用入口的方法。
3. B 线继续 B02：实际 projection/bootstrap/event 和 handler 组合响应（team/approvals/content/timeline）DTO、负例与 handler fixtures；完成后 coverage 才能从 pending 改状态。
4. 裁决 siwc/dsaccount 的 Linux at-rest 测试（见 §4）：keyring 集成或平台门控。
5. 新 descriptor 加入时，同步更新 Rust descriptor/runtime 双向测试、exporter ServiceMap 生成与 fixtures；ServiceMap 生成已通用化，只需在 `TYPED_METHODS`/accounts `TYPED_METHODS` 加条目并补 fixture。
