# 后端 → SDK/Electron 交接（2026-10-24 第三轮终稿）

- 对应规范：[`spec-electron-sdk.md`](spec-electron-sdk.md)；覆盖矩阵：[`electron-sdk-backend-coverage.md`](electron-sdk-backend-coverage.md)。
- 任务线状态：**B01 完成；B02 主体完成（bootstrap/team/approvals.admin/content.upload/timeline/device/download 已收口）；B03 完成 28/60 方法。** 仍不是完整 Electron SDK 契约，也不是发布版 artifact。
- 后端 revision：`b7532cc`（CI run `40078332289` **success**，5m21s）。工作树另含其他任务线的未提交改动（config/gate/agent/new-tech 文档），不构成本契约的一部分；导入产物以本 revision 的干净检出为准。
- 契约 contract hash：`12f8467cfd814a446988f39f081ca71301ae6236e769d86430c1b2f91e0055f4`（dev/release 同树一致：manifest 自身不参与哈希；hash 是完整性标记，不是运行时连接条件）。第三轮产物 278 个文件、coverage 37 implemented / 42 pending；第四轮（本切片）后 coverage **46 implemented / 33 pending**，产物份数与 hash 以重新导出为准。
- 契约 owner：后端；SDK 必须从 exporter 产物导入，不能自行补齐 DTO、权限或 fallback 形状。

## 1. 本日三轮累计交付

| 轮次 | commit | 内容 |
| --- | --- | --- |
| B02 基础 + B03 首切片 | `35264bd` | `qaqh-contract-export` 工具（staging/manifest/export→check）、CI workflow、`just sdk-*` 入口、`check-sdk-contract.py`、wire 根类型 ts derive、`daemon.version`/`session.rename`/`fs.*` typed descriptor |
| B03 账号面 | `3882b1d` | siwc 8 + dsaccount 10 项 typed descriptor 与逐方法 DTO 收口（DTO 所有权在 `qaqh-{siwc,dsaccount}/src/service.rs`），`service.rs` 旧分支删除，ServiceMap 生成通用化 |
| CI 修复 | `992c862` | 补提交 exporter 依赖的 `pending_store` 收据元数据（`ReceiptPolicy`/`RECEIPT_POLICY`/`open_persistent_at` + boundary tests）——曾因漏提交导致 CI 红 |
| **B02 具体组合** | `b7532cc` | **见 §2**：bootstrap/team/approvals.admin/content.upload handler 收口为一手 DTO，投影闭包 ts 导出，`ClientV2*` 具体组合别名 |
| timeline/device/download 收口 + `session.list`/`session.meta` | `17bf7c4` | `TimelineSseFrame` 与 timeline wire 锁测试；approvals.device/content.download 契约钉入 coverage（wire_notes）；B04 幂等结论机器可读（`command_idempotency`）；session.list/meta typed descriptor。**该提交遗漏两处依赖**：mutil-ai pin 未同步（CI 红）与 timeline 测试漏 import（见下两行修复） |
| CI 修复 | 本 PR | workflow mutil-ai pin → `ba263b9`（`max_sse_line_bytes`/`max_sse_event_bytes` 只在该版存在）；timeline 测试补 `TimelineTurnState` import |
| **B03 会话面** | 本 PR | **见 §3**：`session.activity`/`session.get_activity`/`session.set_profile`/`session.set_tool_mode` typed descriptor；`SessionActivity` 首次 ts 导出（turn_id 为 `?: string`，wire 缺键一致）；`stats::activity` 收口为一手 `SessionToolActivity` |

SDK 侧（F 线仓库 `../qaqh-sdk`，非 git 管理）：`src/protocol/generated` 已按 README 约定由 exporter 直接导入本 revision 产物；`pnpm typecheck/test/build` 全过；coverage 守卫测试已更新到新基线（bootstrap-watch/team/approvals.admin/content.upload 为 implemented，timeline/device/download 显式 pending）。

## 2. B02 具体组合类型（本轮核心增量）

### 所有权与层级

- **`qaqh-session/src/client_v2.rs`** 是 bootstrap/team 具体组合的唯一权威：`ClientV2ControlState`、`ClientV2ToolState`、`ClientV2TeamResponse`（+ `TEAM_SCHEMA` 常量）。daemon bootstrap handler、`qaqh-client`（re-export 保壳层命名）、TS 导出共用这一份定义；daemon 局部镜像与 client 重复定义已删除。
- 层级：session→ringing 为**合法向下依赖**（client_v2 引用 `RingingV2PendingInteraction`/`RingingV2DriverState`）；`qaqh-ringing` 永不反向依赖 session。
- 投影闭包补 feature-gated ts derive：`ConversationSnapshot`（及其 10 个组成类型）、`TeamSnapshot`、`TaskBoardSnapshot`/`TaskView`、`TeamId/TaskId/TeamActor/TaskState/TaskArtifact`。TS 类型文件 ~55 → **~196**。

### `@qaqh/sdk/protocol` 新可用类型

```ts
// index.ts 生成的具体组合别名（泛型容器不再以 () 占位）
export type ClientV2Bootstrap = RingingV2Bootstrap<ClientV2ControlState, ConversationSnapshot, ClientV2ToolState>;
export type ClientV2Event = RingingV2EventEnvelope<ProjectionPayload>;
export type ClientV2Payload = ProjectionPayload;   // 完整 delta 词表闭包

// approvals admin 面（GET /ringing/v2/sessions/{id}/approvals）
RingingV2PendingApprovals          // { pending_permission: …|null, pending_interaction: …|null }，键恒在
RingingV2PendingPermissionView     // tool_call_id + 动态字段（unknown/JSON value）+ details_unavailable
RingingV2PendingInteractionView    // { id, kind: "ask"|"plan"|"permission", details: unknown|null }

// content upload（POST /ringing/v2/content）
RingingV2ContentUploadResponse     // { content_id, media_type, sha256, size, truncated }
```

约束：approvals 的动态字段（tool_name/paths/…）契约只承诺「键存在、值为 JSON」；device 审批是**另一种 opaque challenge 形状**，未与 admin 面混用；ephemeral 事件不得携带 cursor/log_id/fact_seq/projection_index/revision（envelope `validate()` 强制，fixture 覆盖）。

## 3. Service 面（B03 累计 28 项）

`ServiceMap` 由两份 descriptor 清单（`ringing/service_methods.rs::TYPED_METHODS` + `accounts.rs::TYPED_METHODS`）生成，无手写方法名单：

| 组 | 方法 | 元数据要点 |
| --- | --- | --- |
| 通用 | `daemon.version`、`session.rename`、`fs.list`、`fs.read` | `fs.*` 的 `scope_session_id` 是可选收窄参数（非 lease session）；`max_bytes` 缺省 512 KiB、上限 8 MiB；allowed-roots 检查仍在 handler |
| 会话面 | `session.list`、`session.meta`、`session.activity`、`session.get_activity`、`session.set_profile`、`session.set_tool_mode` | `session.activity` 结果条目即 `qaqh_domain::SessionActivity`（与 `/activity` 端点同载体）；`get_activity` 条目 `SessionToolActivity`（`args` 为 JSON 字符串原样保留）；`set_profile` 的空 `name` = 清除选择；`set_tool_mode` 的取值/组合校验在 handler |
| siwc（8） | status/profiles/models/sign_in.start/sign_in.status/sign_in.cancel/profile.select/sign_out | 全部 admin_only；DTO 在 `qaqh-siwc::service`（camelCase 服务面、金额/URL 一手形状） |
| dsaccount（10） | status/balance/bonuses/sign_in.start/sign_in.status/sign_in.cancel/sign_out/bonus.ack/models/verify | 全部 admin_only；金额原串（`0E-16`）、snake_case 轮询面；`models` 经 `{id, ..flatten}` 投影，缺 id 显式失败 |

账号面运行时单一收口在 `service_methods/accounts.rs`（params 解码→调用管理器→DTO 投影，drift 报错）；runtime `ts` feature 转发至两个账号 crate。空 struct 生成 `Record<symbol, never>`，已验证严格 tsc 可传 `{}`。

## 4. Fixtures（全部由真实 Rust DTO serde 往返生成）

wire 基础（open/command/ack/status/reset/driver/pair/devices/receipt-policy）、service 22 项 ×params/result、账号 36 份，以及本轮新增：`bootstrap-response.json`、`team-response.json`、`approvals-pending.json`、`approvals-empty.json`、`content-upload-response.json`、`event-envelope-team-delta.json`。无真实 token/路径/账号/正文。

## 5. 生成、校验与门禁

```bash
just sdk-export <dir> && just sdk-check <dir>          # 开发产物
just sdk-verify --typescript                            # 独立 tsc 消费者 + 摘要/确定性（tsc 需已在 PATH）
python3 scripts/check-sdk-contract.py --release         # 发布门禁（拒绝脏树/缺 revision）
```

CI（`.github/workflows/sdk-contract.yml`，PR/push 触发）：ringing/runtime(ringing::)/daemon(v2_) 测试 → `check-sdk-contract.py --typescript --release` → 版本锁定 artifact 上传。本 revision 已实跑通过。

## 6. 已执行验证（第三轮，全绿）

- `cargo check --workspace --all-targets`；`cargo check -p qaqh-session --features ts`
- `cargo test`：session lib 67、client lib 71、daemon 90（bootstrap/team/approvals/content handler 已走 DTO）、ringing lib 19、runtime lib(ringing::) 100、exporter 6
- `check-sdk-contract.py --typescript`（消费者正/负例覆盖 ServiceMap 账号方法、bootstrap 组合、approvals 必备键、upload 字段）
- 产物导入 `../qaqh-sdk`：`check --against` 一致 + `pnpm typecheck/test/build` 全过
- CI：run `40078332289` success

既有平台性失败（非本线引入，依赖图不含本线 crate）：`qaqh-siwc` 2 项、`qaqh-dsaccount` 3 项 at-rest 加密断言在 Linux 必然失败（`qaqh-config::secrets` 非 Windows 为明文 + 0600，TODO keyring）；CI 测试范围不含这两个 crate 的 lib，不阻塞门禁。修复需 owner 裁决。

## 7. 未交付 / F 线不可假定

- timeline 快照/分页/watch：`TimelinePage` 仍在 `qaqh-client` 且无 ts 导出，handler fixtures 未覆盖——coverage pending。
- device 审批 challenge 视图（opaque 形状）与 content 下载 bytes/header 契约未收口；远端/TLS 验收延后。
- 其余 32 个 `METHODS`（`session.dashboard` 与 workspace/config/profile/todo/plan/git/skills/diagnostics/subagent）无 typed descriptor；禁止 `any`/调用者自选泛型伪装。`session.dashboard` 的结果内嵌 todo 条目形状，应随 todo 切片一并收口，不单独镜像。
- tarball 独立安装、Electron IPC 消费、daemon/Electron 真机联调未做；B04 幂等作用域结论未出（boundary tests 已绿，机器可读结论待整理）。

## 8. 下一步

1. **F 线（解锁项）**：F03/F04 可从 mock 切换到真实类型——`ClientV2Bootstrap`/`ClientV2Event`/`ClientV2Payload` 及完整投影闭包已就位；SSE decoder/恢复语义按 spec §5.3。
2. B 线 B02 收尾：timeline DTO/fixtures；device challenge 与 content download 契约随 F 线联调定稿。
3. B 线 B03：会话面已收口（activity/get_activity/set_profile/set_tool_mode）；下一批按 Electron 实际调用入口优先级选（workspace 与 git 组各为整组收口；`session.dashboard` 等 todo 形状先钉 `todo.*` descriptor）。
4. B04：把 pending_store boundary 测试结论整理成机器可读的幂等作用域说明（F04 重试语义依赖）。
5. 裁决 siwc/dsaccount Linux at-rest 测试（keyring 集成或 `cfg(windows)` 门控）。
6. 新 descriptor：在两份 `TYPED_METHODS` 加条目 + fixture 即可，ServiceMap/coverage 自动跟进。

收到契约漂移、DTO 不可用或 TS 形状问题时，请附失败 fixture/编译错误回 B 线；SDK 不加多形状 fallback。
