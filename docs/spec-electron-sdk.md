# SDK-ELECTRON-01：Ringing TypeScript SDK 与 Electron 接入规范

- 日期：2026-10-24。
- 状态：待实施；本文定义目标与验收，不表示实现、测试或发布已经完成。
- 工作仓：`qaqh-backend`、新增同级 `qaqh-sdk`、`qaqh-electron`；`qaqh-desktop-app` 仅在共享渲染层确需适配时修改。
- 施工模式：后端线 B 与客户端线 F 并行；每个任务 ID 一个聚焦 PR，跨仓联调由 J 任务收口。
- 当前范围：以实际产品代码的 Ringing wire version 2、HTTP/SSE、lease/driver 为基线。本文不是 Ringing major 3、客户端工具宿主或整个 UI 重写方案。
- 规范用语：MUST/必须为验收条件，SHOULD/建议为允许有说明的偏离，MAY/可为可选实现。

## 0. 开工入口与边界

### 0.1 两条线现在各做什么

**后端负责人：**阅读本文，领取 B01，输出当前协议覆盖矩阵；随即推进 B02/B03，交付机器可消费的契约目录。不得替客户端写第二份 TS 协议或改 Electron 壳。

**SDK/Electron 负责人：**阅读本文，领取 F01/F02，建立 `qaqh-sdk`、SDK 本地接口、传输测试装置与 Electron bridge 边界。B02 到齐前可以通过 transport mock 开发，但不得把猜测的 wire 类型当正式契约；导入真实生成物后才可合入依赖 wire 的功能任务。

**双方最早交接：**B01 的覆盖矩阵、导出目录结构、首批 B02 产物；不必等待所有 service DTO 完成。首批必须覆盖 connect → attach → bootstrap/watch → send → command status → disconnect。

### 0.2 仓库约束

后端先读 `AGENTS-x.md`。其引用的 `docs/ARCHITECTURE.md`、`docs/spec-architecture-convergence.md` 在本文编写时不存在；不得为了满足链接而伪造架构文件或覆盖后续出现的文档。当前长期设计入口为 `../new-tech/docs/README.md`（相对本文件）；目标协议与当前运行协议必须区分。

本文采取最保守的兼容边界：不新增 HTTP 路由、不改变现有 wire 字段、大小写或业务语义；增加 Rust DTO、TS 导出和 SDK 包装不等于改变 wire。发现确需改变路由、授权、终态或 wire 语义时，提交具体阻塞和 ADR 交所有者裁决，不得由任一施工线自行升级协议。本文不授予绕过现有架构约束的权限。

必须保留 canonical facts/blob 为唯一会话权威，不新增 SDK 持久化事实库、不让订阅回调触发 resume 等业务副作用、不新增会话级 static/TLS、不持锁跨 IO/回调。公开 Rust 跨 crate 错误必须具名；新依赖边必须说明。超过 1500 行的既有文件不得继续增长，新文件不得超过 1000 行。

### 0.3 完整交付与非目标

本轮“完整”指：§4 所列当前 Electron 产品能力均有明确契约、SDK 实现和验收，没有以空数组、空对象、假在线状态伪装成功的占位实现；并完成真实 daemon 与 Electron 联调。

不在本轮：强制 Tauri/TUI 改用 TS；Rust FFI/N-API；Android/iOS/Harmony 原生 SDK；任意版本互通；major 3/WS 重构；市场安装/客户端工具执行；新建深历史分页；重写 Solid reducer；新增自动模型调用或访问真实账号的 CI 测试。

配对、device、远端 TLS 的既有 DTO 必须可导出。远端连接管理与移动端 UI 不作为本轮本地 Electron 发布门槛，API 必须明确其支持状态，不得对外宣称远端已验收。

## 1. 已核对的代码起点

以下仅是静态阅读事实；行号辅助定位，以符号为准。实现必须增加行为测试，不得把源码字符串匹配作为功能正确的证明。

| 起点 | 证据 | 本轮处理 |
| --- | --- | --- |
| Rust 已有共享客户端 | `crates/qaqh-client/src/lib.rs:1–9` | 保留，作为行为对照，不在 Node 中另包 Rust 运行时 |
| domain command、projection payload 已有 TS derive | `crates/qaqh-ringing/src/command.rs:17–25`；`crates/qaqh-session/src/session_fact_v2/projection_event.rs:298–310` | 复用，不重抄 |
| 握手、v2 envelope/bootstrap 等未配 TS derive | `crates/qaqh-ringing/src/v2/types.rs:17–69,170–201,397–415,449–467` | B02 补全公开 wire 类型的传递闭包 |
| 具体 bootstrap/event、timeline DTO 位于 Rust client | `crates/qaqh-client/src/v2.rs:146–183`；`crates/qaqh-client/src/types.rs:81–139` | 导出真实组合或最小职责迁移，不制造 TS 镜像 |
| service 表有权限元信息，请求枚举与返回值尚未统一成映射 | `crates/qaqh-runtime/src/ringing/service_methods.rs:17–31,92–171`；`crates/qaqh-client/src/client.rs:400–420` | B03 建 typed descriptor 与校验 |
| 当前 ts-export 由 Tauri 仓写入自身 src/api | `../qaqh-desktop-app/justfile:66–83`（从 backend 根解析） | 后端提供独立导出入口；旧入口最终转调统一入口 |
| Electron 手写 ACK、使用 any 拼命令 | `../qaqh-electron/src/main/types.ts:15–21`；`src/main/ringing-client.ts:184–214`（该仓） | F03/F04 替换 |
| Electron attach 发送 session_resume | `../qaqh-electron/src/main/ringing-client.ts:131–148` | attach/resume/watch 分离 |
| Electron events 直接裸订阅，reset 仅转发 | `../qaqh-electron/src/main/ringing-client.ts:303–344` | F03 建 bootstrap/cursor/reset 恢复 |
| SSE 累计 data 在每 chunk 内初始化 | `../qaqh-electron/src/main/ringing-client.ts:415–447` | 先写跨 chunk 红测试再替换 |
| bootstrap IPC 使用 activity 并吞错 | `../qaqh-electron/src/main/ipc-handlers.ts:213–215` | 接真实 bootstrap，不保留兜底 |
| approvals IPC 调 service 并吞错 | `../qaqh-electron/src/main/ipc-handlers.ts:81–84` | 接真实审批契约 |
| 后端 admin/device 审批形状与应答路径不同 | `crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs:349–443` | 显式分型，不混用 |

## 2. 所有权与架构裁决

| 事项 | 唯一所有者 | 禁止 |
| --- | --- | --- |
| wire DTO、服务方法与权限、后端终态语义 | backend | SDK 自造 wire 字段/权限/终态 |
| TS 生成物的生成器和内容 | backend | Electron/SDK 手改 generated |
| 生成物导入、npm 公共 API、TS 连接行为 | sdk（客户端线负责） | 消费者安装时跑 cargo |
| 本地 discovery 读取/校验/重新解析 | sdk/node | core 直接依赖 fs/process/Electron |
| daemon 包路径、spawn/复用/退出策略 | Electron | SDK disconnect 顺带 kill daemon |
| 窗口、托盘、截图、文件选择、系统浏览器 | Electron | 进入协议 SDK 或 daemon 新 API |
| renderer 调用授权、challenge 与窗口绑定 | Electron main bridge | 只相信 TS 类型或任意通用 RPC |
| 会话状态事实、审批有效性、实际授权 | backend | UI/SDK 缓存代替后端裁决 |
| UI 投影渲染和 UI 偏好 | 共享渲染层 | SDK 复制整个业务 reducer/无限消息库 |

首版 npm 包定名 `@qaqh/sdk`（发布命名空间不可用时只更改包名，不改变职责），独立仓 `qaqh-sdk`。一个包、三个公开子路径：

```text
@qaqh/sdk             客户端、高层 API、SDK 本地状态与具名错误
@qaqh/sdk/protocol    后端生成的公开协议类型/常量/方法映射
@qaqh/sdk/node        Node 本地 discovery 与凭据解析器
```

建议目录：`src/protocol/generated/`、`src/core/`、`src/core/streams/`、`src/node/`、`test/conformance/`、`test/integration/`。core 禁止 import Electron、Tauri、Solid、Node fs/process；Node 依赖只能从 node 子路径进入。包提供 JS、声明文件和 exports，使用 pnpm 锁文件，消费者无需 Rust 或源码邻接目录。core 可以注入 Web 标准 fetch/stream；Node 特有传输能力必须留在适配层。

## 3. 双方交接协议：机器产物与生成纪律

### 3.1 后端唯一导出入口

B02 新增构建期工具 `qaqh-contract-export`（推荐 `tools/qaqh-contract-export` workspace member）。它聚合既有类型，不被 runtime/daemon 反向依赖；如放置路径因现有规则需调整，由 B01 记录，外部命令与目录布局不得随意变化。

约定命令（B02 首批实现；完整契约覆盖仍待 DTO 接线和 fixtures）：

```bash
cargo run -p qaqh-contract-export -- export --out <contract-dir>
cargo run -p qaqh-contract-export -- check --against <contract-dir>
# 发布门禁增加 --release，并注入 QAQH_SOURCE_REVISION=<revision> QAQH_SOURCE_DIRTY=0
```

`export` MUST 在独立 staging 目录生成并成功校验后再交付；不得删调用者给出的任意目录或留下半套产物。`check` 必须重新生成比较，差异/缺文件/多余陈旧生成文件时非零退出。传递后端 revision 由发布编排获取并通过 `QAQH_SOURCE_REVISION` 注入；工作树状态通过 `QAQH_SOURCE_DIRTY` 注入（仅值 `0`/`false` 标记干净，缺省保守标记 dirty），不从文档日期猜 revision；脏工作树不得标为已验证的干净发布版本。当前工具为首批实现：正式发布仍须确认可信 revision/dirty 注入，并完成覆盖表中全部 MUST 对应 DTO/fixture。

交付布局固定：

```text
contract-dir/
  manifest.json
  types/                       ts-rs 类型，保持 import 可解析
  index.ts                     公共类型出口，不 export 内部事实存储接口
  constants.ts                 schema、wire version、header 等编译期常量
  service-map.ts               ServiceMap + 精确的方法元数据
  fixtures/                    合成数据；不得含真实 token/路径/账号/会话正文
  coverage.json                路由/服务/类型/fixture 覆盖矩阵
```

`manifest.json` 至少包含 `format_version`、`wire_version`、`backend_version`、`source_revision`、`source_dirty`、`generator_version`、`contract_hash`、`files`。`files` 为按 POSIX 相对路径排序的数组，每项固定键顺序 `{ "path": "...", "sha256": "小写十六进制" }`，记录除 manifest 自身外每个产物的字节 SHA-256；`contract_hash` 为该数组无空白、无尾换行的 UTF-8 JSON 编码的 SHA-256 小写十六进制，不包含时间戳与绝对路径。版本/revision 由构建流程提供，缺失来源只能标为开发产物并禁止正式发布。相同输入和元信息必须得到字节一致的输出。

B02 首批产物仍保留固定目录与文件名：`service-map.ts` 可只包含已正式交付的方法或空映射；未交付项必须在 coverage 标 pending。B03 扩充同一套产物，不另建第二种包格式。export/check 所需 revision 输入渠道与 importer 校验样本在 B01 交接中固定，避免两线各猜环境变量。

现有 ts-rs 的 number 配置必须集中固定；不可让每个消费者自行设置环境变量改变 wire 数字的 TS 类型。大整数须对应后端已承诺的安全整数范围；不能为“更安全”把现有 JSON number 擅改成 string。金额原串保留为 string。

### 3.2 真实契约，不是导出专用镜像

- discovery 文件的公开形状，以及握手、renew、envelope、reset、bootstrap、command/result/status、driver、timeline、content、team、approvals、pairing/device 的公开类型及依赖闭包必须可导出。discovery 的 token 字段只在宿主消费，不因类型公开就向 renderer 暴露。
- 泛型 envelope/bootstrap 需提供当前产品真实具体组合的公共类型名。cursor 对 TS 消费者是不透明字符串；不要求 UI 解码/构造 canonical cursor。
- handler 中用 `json!` 拼出的稳定响应，应提取实际被 handler 使用的 DTO；不得只增加一份“看起来一致”的导出 struct。
- 现有 client DTO 可加 feature 门控导出或最小迁到正确的契约模块。不得令 `qaqh-ringing` 反向依赖 session/runtime，也不得为了导出把全部 client/实现类型公开。
- serde tag、rename、skip/default、缺键与 null、混合 camelCase/snake_case 必须原样保留。未知扩展字段可忽略；必要字段缺失、错误判别式、越界数字必须可观察地失败。
- ts-rs 不生成运行时验证器。首版允许 SDK 编写边界 decoder，但只能按导出契约实现并由 Rust fixture/负例测试约束；禁止 Electron 再维护第三份 validator。自动生成运行时 schema 可后续单独交付，不是隐藏前置条件。

### 3.3 service 方法关联

B03 为公开 service 建立一个可遍历的 typed descriptor 来源，表达每个方法的 `params/result/kind/requires_session/admin_only`；Rust 声明必须关联实际 request/response DTO，生成 TS `ServiceMap` 与方法元数据。元数据不得在 runtime、exporter、Electron 各手写一套无校验名单。

可渐进接入现有 handler，但本轮要暴露给 SDK 的方法必须实际受 DTO 验证/序列化约束，并有 descriptor ↔ runtime 方法表的双向覆盖测试。`fs.*` 的 scope 参数等额外要求必须显式建模，不能只靠 `requires_session` 一个布尔值猜测。

对 session 生命周期仍只走 command；不得为方便 SDK 将其复制成 service。进程诊断等敏感方法可在 SDK 具名 admin 子面提供，但 bridge 必须单独显式放行。客户端 allowlist 是宿主约束，不替代后端权限检查。

目标类型效果（示意，具体 DTO 名由生成器导出）：

```ts
client.service("session.list", {}); // 返回类型从 ServiceMap 推导
```

禁止使用 `service<T>(method: string, params: any)` 让调用者自报正确性。服务正常字段中的 JSON 动态值可用 `unknown`/JSON value，但必须在 DTO 指定位置，不能以 any 覆盖整个响应。

### 3.4 导入与版本

后端产物是权威，sdk 将审核过的产物原样导入 `src/protocol/generated/`，manifest 随包发布。B 线发布目录或 CI artifact 给 F 线；F 线校验摘要与覆盖，不重跑后端生成逻辑。短期允许本地路径传递 artifact，但正式 SDK/Electron 构建不得依赖兄弟仓源码。

SDK 自己实现的业务包装与公共命名由 F 线维护。修改导出物必须回 B 线；禁止“临时手修 generated 后再说”。Electron 固定 SDK 版本，打包记录 daemon revision + SDK version + contract hash；运行时以 wire version/capabilities 判断兼容，不能要求契约 hash 相等才连接。

## 4. 功能覆盖与完成口径

B01 输出覆盖矩阵初稿。生成的 `coverage.json` 只记录后端可验证的路由/方法、params/result 类型、权限、fixture 和任务状态，不依赖尚未发布的 SDK，避免产物循环依赖。F 线另维护 SDK/Electron 消费与测试记录；J01 在发布报告中按同一能力 ID 合并双方矩阵，不回写已发布的契约包。联合矩阵每项必须记录：功能、SDK API、路由/方法、params/result 类型、授权档位、fixture、集成测试、状态。状态可为 pending / implemented / blocked / out_of_scope；blocked 必须带任务 ID 和原因，不能标 completed。首批部分产物的 pending/out_of_scope 必须显式列出，不能拿部分产物发布“完整 SDK”。

| 能力组 | 本轮 MUST | 主要后端入口 |
| --- | --- | --- |
| 连接 | discovery、open、capabilities、renew、重新协商、disconnect | clients/open、leases/renew |
| 会话目录与生命周期 | list/activity、create、attach、resume、close/archive/unarchive/delete、rename | typed service + control commands |
| 消息与回合 | send、cancel、compact、模式/工具模式及当前 UI 支持的输入选项 | conversation/control commands |
| 观测 | bootstrap、projection watch/reset、timeline snapshot/page/watch/gap、状态查询 | sessions/{session_id}/bootstrap、events、timeline、timeline/events |
| 命令 | ACK、查询状态、existing/result、原 ID 对账、revision/driver guard | POST commands/{channel}；GET commands/{command_id} |
| 交互 | pending、ask/plan/permission 分型、应答、dismiss、过期/重复/正文不可用 | approvals + canonical commands |
| 内容 | Uint8Array 上传/下载、类型化 ContentRef、大小/摘要/错误处理 | content、content/{content_id} |
| 配置与组织 | config/profile/workspace 与现有 UI 的 fs/git/todo/plan 操作 | 精确 ServiceMap |
| 账号 | siwc/dsaccount 当前公开方法、状态和错误，不回传密钥 | 精确 admin service |
| 团队 | team/任务板/消息板快照与现有单流增量 | sessions/{session_id}/team + projection |
| 管理 | 当前 UI 使用的 memory diagnostics、设备列举/撤销、显式 stop API | 既有 admin 入口；默认不向普通 bridge 放行 |
| 平台 | 文件选择/截图/用户 skills 扫描/托盘/窗口/系统浏览器 | Electron 本地，非 wire SDK |
| 延后 | 配对/设备 DTO 导出；远端 transport/TLS 的真实连接验收延后 | 不冒充本地 admin 已覆盖 device 行为 |

以上不是授权新增功能。某操作当前 UI 没有入口也不能编造实现；按后端现有公开能力包装，B01 明确最终清单。timeline 的 `truncated_before` 必须透传并展示可识别状态，不能制造新深分页 API 或不停请求不可达历史（现有定义见 `crates/qaqh-client/src/types.rs:94–114`）。

## 5. SDK 行为契约

### 5.1 生命周期与接口形态

一个 client 对应一个明确 endpoint 身份上下文；多个 session handle 使用显式 `session_id`，core 不设隐含全局 active session。Electron 可有窗口级 active session，但写操作必须显式解析目标，不得漏参就静默操作另一个会话。

公共接口形态冻结如下（示意，不是现成可运行的实现；具体参数类型从协议导出，不手抄）：

```ts
const client = createClient({ credentials, fetch, logger });
await client.connect();
const session = client.session(sessionId);
await session.attach();
const watch = await session.watch({ projection: true, timeline: true, onUpdate });
const submission = await session.sendMessage({ text });
const status = await client.commands.status(submission.commandId);
watch.dispose();
await client.disconnect();
```

`attach()` 只建立归属；`resume()` 是显式 actor 生命周期命令；`watch()` 不隐式 resume/claim driver。自动恢复时只重建必要归属/订阅，不重建 actor。依据 `ControlCommand::SessionAttach/SessionResume`（`crates/qaqh-domain/src/command.rs:64–77`）。

client 状态至少区分 idle、connecting、ready、recovering、closed；“有 active session”不代表 ready。订阅状态按 session/stream 独立维护。并发 connect/open/recovery 合并为单次操作；disconnect/dispose 幂等，释放所有 timer、reader、pending wait，之后迟到回调不得继续交付。disconnect 默认不关闭会话、不停 daemon。

### 5.2 凭据与 lease

凭据解析器区分 local discovery 与 explicit endpoint；远端错误不能改道到本机。重新发现后 endpoint/token 可以更换，但只能在连接边界使用新凭据；日志/错误对象不得带 token。请求和 open/renew 都要有有限超时；SSE 不沿用普通请求总超时，使用独立 idle timeout。

续约依据协商的 interval/TTL 安排，禁止固定写死 10/15 秒；防止 setInterval 异步重叠。失败触发有界退避与 single-flight 重新协商；新 lease 发布后旧流取消并用新上下文恢复。server epoch 改变使旧 cursor/待处理恢复上下文失效，必须重新定基，不把旧 ACK 不确定性冒充执行失败。view/interact/drive 等能力拒绝要清楚，不能自动升级权限。

### 5.3 快照、游标与可靠交付

- projection 首次先 bootstrap，向订阅者交付该基线，再从 snapshot cursor 订阅；snapshot 与订阅之间的事实必须可续传。不能裸订阅后假定没有遗漏。
- cursor 仅按可靠事件推进；replaceable/ephemeral 不推进可靠位置。cursor 视作 opaque，不在 UI 中按字符串排序。
- 交付到 SDK 订阅边界并成功接受后才推进对应消费位置。可靠事件按源顺序处理；重复可去重但不得漏掉同 fact 下不同 projection_index 的事件。
- reset 后进入 resync，停止旧基线增量交付，重新取权威基线并恢复；不只向 UI 广播 reset 就继续旧流。
- timeline 独立维护 watermark/Last-Event-ID，按后端规则检测 gap，补取 snapshot。两种 cursor 不混用，不承诺两种流之间存在全局到达顺序或原子快照。
- 同一会话/流由 SDK 管理一条底层连接，多订阅引用计数；每个消费者先取得自己的基线。缓冲期间取消/基线失败要收尾。
- 队列按数量/字节有界；replaceable 可按语义合并，ephemeral 可按策略丢弃，可靠事件溢出必须显式进入恢复，不能 warn 后继续假装完整。
- 回调抛错、renderer 销毁或慢消费者必须隔离；失败订阅不得影响其他会话。重连/刷新后的消费者必须拿新快照，不能仅拿未来事件。

SSE decoder 必须跨任意网络 chunk 保存完整 frame 状态，覆盖 UTF-8 分片、LF/CRLF、多行 data、注释心跳、id/event、EOF/半帧、取消和超大帧。超大帧/非法关键 JSON 显式报协议错误并进入受控恢复；不能静默跳过可靠数据。退避有上限、可取消，测试可注入时间/随机性，禁止无延迟循环。

### 5.4 命令、幂等与终态

SDK 自动分配 command_id，也允许调用者显式提供。同一操作的重试保留 ID、原业务 payload 与相关 guard，不得重试时重造 ID 或默默去掉 expected_revision/driver_epoch。新 lease 后是否可原 ID 重放，按 B04 验证过的幂等作用域决定；未证明安全时只查询/报告 outcome unknown。

返回值必须区分 ACK rejected、ACK accepted、最终 succeeded/failed/rejected、结果未知。accepted 不能被 UI 当完成；最终结果来自已确认的 command status/可靠 causation 事件。不能为任意事件相似文本宣布完成。现有 typed result 只覆盖部分交互，不承诺所有命令都有相同 result payload（`crates/qaqh-ringing/src/v2/types.rs:586–632`）。

等待超时/取消意味着“不再等待”，不意味着后端已取消或操作失败；暴露 commandId 供继续查询。receipt 不存在或超出保留边界时返回明确 unknown，不自动重发有副作用操作。普通 service 写方法默认不重试；读方法只有 metadata 明确无副作用且满足策略才可重试。用户主动 cancelTurn 是单独命令。

### 5.5 审批

本地 admin SDK 使用真实 pending DTO，分别构造 ask、plan、tool permission 的 canonical typed command；permission 可能走 tool channel，禁止把所有审批写成 control/approval_decision。正文不可用保留可识别状态，不捏造“安全”详情。不得从事件的模糊 id 字段猜 canonical interaction/tool call ID。

Electron bridge 将 canonical 标识保留在 main，给 renderer 有界展示视图与 opaque challenge；绑定 client/window/session/interaction kind，限制数量/TTL、去重、单次提交与失效清理。并发答复不能重复放行；提交遇网络不确定性由 main 保留 commandId 对账，不让 renderer 通过复用 challenge 新建另一命令。过期/后端拒绝后重新刷新 pending。

device 审批是另一种后端形状及 `/approvals/respond`，本轮不得与 admin 流混用。后端最终决定 first-answer-wins、权限及有效性。

### 5.6 错误与校验

SDK 具名错误至少区分 transport、timeout、cancelled、protocol、incompatible、authentication/authorization、command_rejected、outcome_unknown；保留后端 code/status/retry_after/commandId 等非敏感字段。保留未知后端 code，不靠 message 文本判断业务分支。

关键边界校验 schema/version、capabilities、epoch/session、判别式、必要字段和安全整数；未知额外字段不等于错误版本。禁止把失败转换为成功的 `[]`/`{}` 或把无响应当 closed/idle 的正常业务结果。前端显示文案不得取代结构化错误。

## 6. Electron 实施边界

### 6.1 main 使用 SDK，preload 不持 daemon 凭据

main 创建与复用 SDK client，窗口订阅有独立生命周期。preload 只公开受限 bridge；renderer 不取得 token、discovery 原文、任意 endpoint/header 或 SDK admin 实例。

第一轮保留 `__TAURI_INTERNALS__` 的 invoke/listen 兼容形状以复用现有渲染产物，不同时重写共享 UI；兼容层只做映射，不能留第二份 HTTP/SSE 实现。UI 需要的最小更改由 F06 列出。公共 SDK 不公开 Tauri 内部名称。

### 6.2 IPC 必须做运行时授权

- 每次调用校验 webContents/frame 来源与窗口状态；信任的本地页面策略明确，子 frame/外部导航默认拒绝。
- 明确命令、service 方法与参数白名单；禁止任意 service 字符串透传、账号前缀通配放行。
- session 目标明确，admin diagnostics/stop/device 管理单独授权，不由普通 RPC 绕过。
- 本地附件必须来自宿主授予的选择结果/能力句柄；renderer 不能提交任意路径令 main 读出并上传。URL 打开使用 scheme/目标策略，禁止执行任意 shell。
- IPC 数据可结构化克隆且有大小上限，错误转换为具名结构；不直接序列化 Error/秘密。
- 可靠事件向 renderer 交付不假定 webContents.send 等于消费成功：桥要有有界队列与序号/确认或等价的可证明机制；发现 gap/刷新则重新交付基线。不新增 daemon 协议。
- 窗口销毁/取消监听解除 SDK 订阅、回调、challenge 和等待；日志不得打印账号凭据/附件正文。

### 6.3 daemon 管理与退出

Electron 保留 executable 定位、sidecar 打包、spawn/复用、托盘/窗口和系统功能。SDK/node 只提供 discovery/解析能力。启动失败、旧 discovery、进程存活但端点错误都必须可观察；不能只以端口可达判为本应用 daemon。

Electron 退出先释放 client 资源，不默认 kill 被 TUI/Tauri/其他窗口使用的 daemon；显式 stop 按后端现有权限和 idle 策略执行。不能因 SDK 测试需要而改真实用户 discovery 或停止真实 daemon。

## 7. 工作包、文件边界与依赖

### 7.1 后端线 B

| ID | 工作与产物 | 依赖 | 验收 |
| --- | --- | --- | --- |
| B01 | 当前路由/命令/service/权限清单，coverage 初稿，实际 DTO 所有权与幂等作用域记录 | 无 | 与当前 router、command enums、METHODS 双向核对；没有从长期 major 3 文档补猜能力 |
| B02 | exporter、v2/具体 snapshot/event/timeline/content/approval 等类型导出、constants/manifest/首批 fixtures | B01 | §3 的 export/check 可用；首批闭环可供 F03/F04 使用；生成 TS imports 可编译 |
| B03 | service typed descriptor、真实 params/results DTO 接入、ServiceMap、权限元信息、账号 DTO 收口 | B01；产物接 B02 | 覆盖 §4 清单；方法表双向一致；JSON 形状不变 |
| B04 | 故障接缝与一致性测试，命令终态/receipt 范围、lease/epoch/reset 的机器样本和说明 | B02；服务部分接 B03 | Rust 行为测试通过；对无法证明的旧行为明确阻塞，不借 SDK 新增语义 |
| B05 | CI artifact/export check、旧 desktop ts-export 转接方案、打包 daemon 基线 | B02/B03/B04 | 无第二权威生成器；可提供精确版本 daemon + contract 给 J01 |

后端默认修改：相关 Rust 契约模块、handler 的 DTO 接线、exporter、测试与 CI。不得顺便重构 agent loop 或长期 major 3。旧 Tauri 导出脚本的实际变更由 B05 发出小补丁、F06/仓所有者合入，避免两线同时改同文件。

### 7.2 客户端线 F（SDK 与 Electron，可再分两人）

| ID | 工作与产物 | 依赖 | 验收 |
| --- | --- | --- | --- |
| F01 | qaqh-sdk 脚手架、exports、严格 TS、pnpm、artifact importer、SDK 本地接口 | 无；协议导入接 B02 | 包可 pack/独立安装；generated 不手写；core 无壳/Node 依赖 |
| F02 | mock transport/clock、SSE decoder、有界队列、Node discovery、typed errors | F01 | 分片/取消/超限/本地与显式连接隔离测试通过，可与 B02 并行 |
| F03 | client/lease/recovery、session attach/watch、两种流、reset/多会话 | F02 + B02 | §5.1–5.3 故障测试通过，自动恢复不 resume |
| F04 | command tracker、driver、ask/plan/permission、content 高层 API | F02 + B02；恢复联调接 F03/B04 | accepted 与终态分离，原 ID 对账、权限/通道正确 |
| F05 | typed service 与 §4 其余高层功能、fixtures 一致性、能力边界 | F02 + B03 | 无调用者自报返回类型；覆盖矩阵无未声明空洞 |
| F06 | Electron main 接 SDK、IPC/challenge/文件能力、兼容 bridge、清理与有限 UI 适配 | F01 可先做桥框架；实接依赖 F03/F04/F05 | 不再手拼 wire，不暴露凭据；多窗口/刷新/退出与现有 UI 回归通过 |
| F07 | pack/版本/独立消费样例、SDK 使用说明、旧客户端删除 | F03/F04/F05/F06 | Electron 从固定包构建；删除旧 RingingClient/重复 wire/state machine；无永久双轨开关 |

F 线拥有 qaqh-sdk 和 Electron。共享渲染层变更只允许修正受验收影响的消费契约，不顺手搬整个前端。一个人可串行完成 F，B/F 两线仍并行。

### 7.3 联合收口 J

| ID | 事项 | 依赖 | 通过条件 |
| --- | --- | --- | --- |
| J01 | 固定 backend revision + SDK version + Electron revision 联调，确认 coverage 与权限 | B05 + F07 | §8 全部 MUST 有可复现记录；缺项有阻塞，不以 mock 通过替代真机 |
| J02 | 导出单一来源、迁移旧路径删除、发布与兼容声明 | J01 | SDK/包/daemon 配对可重现；旧手拼协议退出，未支持远端明确声明 |

并行顺序：`B01 → B02 → F03/F04`；`B01 → B03 → F05`；`F01 → F02` 与上述后端工作同时进行；F06 的 IPC 框架可提前写，真实替换等依赖到齐。B04 不必阻止 F 的 mock 开发，但阻止未验证重试语义进入发布。不得把 B03 全量完成设为 F01/F02 的前置。

### 7.4 交接与冲突规则

每个 PR/交接提供：任务 ID、实际 revision、依赖任务版本、产物目录/摘要、变更的公共类型、已运行命令/结果、未验证项与下一任务。契约漂移先回 B 线处理；SDK 不加字段猜测/多形状 fallback。实际源码与本 spec 冲突时，记录具体符号、行为与最小红测试，交双方裁决后更新 spec，不在一侧悄悄变更。

实现期间不拷贝两份可独立修改的 spec；Electron/sdk 可通过交接引用本文件及版本。新增 API、范围裁剪、远端提级均需更新 coverage 与本文相应条款。

## 8. 测试、探针与发布门槛

### 8.1 后端契约测试

1. export 两次结果一致，check 发现新增/删除/陈旧文件；manifest 摘要与实际文件一致。
2. DTO 实际 serde 形状与 fixtures 一致：skip/default/null、enum tag、混合大小写、安全整数、金额原串。
3. Rust 使用同一 fixture 做 serialize/deserialize/关键 validate；禁止只测试一份独立导出镜像。
4. service descriptor 与 runtime 表双向覆盖，读写/权限/session/scope 元数据相符；账号 admin-only 没有因 SDK 变宽。
5. 并发重复 command、ACK 丢失、receipt 缺失、revision/driver 冲突、审批先答者获胜均有真实边界测试。

### 8.2 SDK 自动化

- 类型：合法调用推导返回值，错误方法/错误参数为编译错误，包声明可被独立消费者消费。
- decoder：逐字节/随机切块、UTF-8、CRLF、多行 data、心跳、半帧/EOF、超限、非法关键字段、取消。
- 恢复：snapshot/subscribe 间插入事实；lease 过期；并发 renew/open；epoch/端口/token 改变；旧回调迟到；reset/gap/重复/同 fact 多投影。
- 资源：重复 attach/watch/dispose、多个窗口与 session、窗口关闭、disconnect 后无计时器/reader/待交付回调，队列内存有界。
- 命令：先来终态再来 ACK、ACK 丢失、原 ID 重放、unknown、等待取消、写 service 不盲重试、审批/driver 能力拒绝。
- 安全：显式端点不读本地 fallback；日志/错误不含敏感值；fixture 负例响亮失败，不变成空数据。

SDK 在 F01 建立稳定脚本：`pnpm typecheck`、`pnpm test`、`pnpm build`、`pnpm test:integration`、`pnpm pack`；integration 允许显式环境变量指定专用测试 daemon，但普通 test 不访问用户机器实例或模型服务。

### 8.3 真实 daemon 与 Electron

使用临时数据目录、临时端口、独立 daemon，不能读取真实 secrets/历史；账户使用受控 mock 或专用可禁用测试装置，不能消耗真实模型额度。真实 transport 用例至少包括：

1. connect → create/attach → bootstrap/watch → 可确定的本地 fixture 产生事实 → 双流收到数据 → 查询/退出。
2. 断开 socket/暂停传输后续传无缺失；实际重启测试 daemon 后发现新端点/凭据/epoch 并重定基。
3. ACK 传输丢失后按原 ID 查询/对账，没有重复业务副作用。
4. 可复现的 reset/timeline gap，UI 获取新基线；多会话看子代理不会触发 resume。
5. ask/plan/permission 三类：pending、详情不可用、应答、重复/过期/跨 session/challenge 拒绝；权限应答不错误要求 driver。
6. 内容上传/下载摘要一致、超限/不存在返回类型化错误；UI 正确区分截断历史。
7. renderer 刷新/慢消费/关闭后主进程内存有界、监听释放；不泄露 token/canonical 审批 ID。
8. IPC 来自外部 frame、任意 service、任意文件路径、错误 session 的调用被拒；普通 UI 不能调用停 daemon/管理账号的任意后门。
9. Linux 实际启动打包应用并完成以上受影响 UI 操作；退出不杀其他客户端共用 daemon。仅 Node 集成测试不能宣称 Electron 真机通过。

### 8.4 验证命令与报告

后端必须记录：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1
```

另加 B02 的 export/check 与契约专用测试。SDK 跑 §8.2 脚本并从 pack 的 tarball 在非兄弟源码布局安装验证。Electron 至少运行其现有 `pnpm run typecheck`、`pnpm run build`，并由 F06 增加 IPC/宿主自动化测试入口；共享渲染层若修改则运行该仓 typecheck/test/build。

缺依赖、平台不可用、既有失败要注明命令、失败原因与影响范围；不得把“未运行”“mock 通过”“文档核对”写成整个功能已验证。J01 发布报告列明每个用例的 backend/SDK/Electron 版本、测试环境、通过/失败/未验证。未完成本轮 MUST 不得打完整 Electron SDK 完成标签。

## 9. 迁移结束与后续扩展

J02 前必须删除 Electron 旧的手写 RingingClient、重复 wire DTO、第二份 lease/SSE 重连逻辑、吞错占位与永久 fallback；保留的 Tauri IPC 兼容层只映射到 SDK，不被宣传为公共 SDK。后端与 desktop 的导出入口最终归一，迁移脚本标明唯一入口，不保留相互覆盖生成目录的双轨方式。

SDK 与 Rust qaqh-client 可以长期并存，因为是两种语言的协议实现，不是两套协议事实源；通过共享 fixtures、覆盖清单与真实 daemon 探针约束行为。未来 major 3、移动端与客户端工具能力另立切片，沿用分层但不得把未来目标冒充本轮已支持能力。
