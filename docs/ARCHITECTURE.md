# RC 前架构与 crate 职责基线

测量日期：2026-10-07。版本基线：2.0.0-beta.3。本文区分当前事实与清理目标，
不宣称目标已经实现。唯一执行清单见 [收敛施工单 §10](spec-architecture-convergence.md#10-clean-rc-前砍刀与-crate-减负)。
机器可读的体量、最大文件和直接依赖见 [clean-baseline.json](metrics/clean-baseline.json)。
复测命令：在仓库根运行 `./scripts/measure-clean.ps1 -OutputPath <输出路径>`，另存结果后
与固定基线比较；不要覆盖基线来掩盖增长。

## 1. 范围和度量

clean 只做架构收敛、旧代码删除、职责迁移、crate 减负及其回归，不加产品功能。
保留多会话、服务端 Loop、ToolRuntime、身份/授权、执行账本、OS sandbox、工作区变化反馈。
不全异步重写，不引入新协议版本，不为了把文件变小而新增 crate。

29 个 workspace crate。表中行数是 src/**/*.rs 的物理行数，包含内联测试、注释、空行，
不含 tests/ 和 build.rs；不等同生产代码行数。依赖数不是编译成本，尚未测编译耗时。
删除、迁移和测试移出 src 必须分别报告，禁止用搬动制造“净删代码”的成绩。

## 2. 当前链路与核心缺口

HTTP/client → daemon 身份与控制权 → QaqhService/Registry → 每 session 一个线程的 Loop
→ ToolRuntime → workspace 授权与工具装配 → 具体工具/sandbox → 结果回填。

当前消息写入为 MessageStore/PersistOp → WAL → SessionManager 归档；canonical 另外写事实；
timeline 有自己的存储与重建。canonical/store.rs 明确尚未接管旧消息写入。
runtime/ringing/timeline_rebuild.rs 从消息与 meta 推算回合号，恢复终态统一为 Completed。
因此 events.jsonl 目前不能被当成完整上下文的唯一恢复来源。

进程内事件仍经过 DomainEvent → RingingEvent → WorkerEventEnvelope → DomainEvent，
actor.rs 消费后触发交互正文、pin 和 activity 副作用。它不是可直接拔掉的空队列。
remote client 连接 daemon 已存在；server 派工具到 client 执行的完整闭环尚未在本仓确认。
clean 不预建一套远端执行协议，该能力缺口独立记录。

## 3. crate 职责裁定

“迁走”表示目标责任归属，实际实施必须同时修改调用者、删除旧导出和旧依赖。
同名类型重新导出可以作为 API 的正常聚合，但专为保旧路径的 shim 不保留。

| 当前 crate | src 行数 | 保留职责 | 删除/迁移裁定 |
|---|---:|---|---|
| qaqh-runtime | 38,339 | Loop、回合/工具调度、实例生命周期、子代理编排 | 设备/pairing/lease/driver 控制状态与 wire hub 归 daemon；timeline 纯折叠与重建归 session；fs/git 展示服务退出 Loop 层。service 初始化移 daemon composition root。拆内部模块先于新 crate。 |
| qaqh-session | 19,023 | 会话事实持久化、blob、提交/恢复、确定性投影、会话与团队归档 | 事实/投影公共契约抽到 session-api；manager 消息直写/WAL 回放随 cutover 删除；工具真正重执行仍由 runtime，存储层只给恢复判定。 |
| qaqh-workspace | 12,188 | 工作区执行装配、工具登记/准入、授权绑定、批次审计协调 | 删除文件/进程/git/permission/SDK 旧路径门面；git 展示调用直接到 git；dashboard 组装归上层；工具实现按既有工具族归位。todo/ask/skill/web 先独立模块，只有形成稳定共用边界才新增工具 crate。 |
| qaqh-file-tools | 11,403 | 文件读写/查询、图片读取、文件修改结果 | 保留文件操作的必要 journal/undo；会话全局/TLS 状态移显式工作区句柄；domain 展示适配移上层。不按文件后缀继续碎拆 crate。 |
| qaqh-daemon | 9,651 | 进程启动/装配、HTTP/SSE、身份、设备、lease、控制权、运行态 hub | 接收 runtime 的传输控制职责，但按 transport/auth/control/service/composition 模块拆；handlers 只解析、鉴权和调用，不能接管业务恢复算法。 |
| qaqh-process-tools | 6,021 | exec、进程管理、检查/等待/终止 | registry 与取消显式绑定生命周期；qaqh-file-tools 依赖逐调用点判断，纯路径/字节辅助下沉 fs-core，不能把文件工具当基础库。 |
| qaqh-message | 5,463 | 模型消息与上下文的确定性变换、压缩/撤回应用 | 删除 WAL、LegacyWriterFacade、重复磁盘写入；共享单一 apply(ContextOp)，不拥有会话磁盘事务。token 计数若迁入此处，provider 不反向进入 message。 |
| qaqh-client | 5,197 | endpoint、连接、命令、重连、客户端 wire 解码 | 消除对持久化实现 qaqh-session 的生产依赖，改依赖纯 session-api；desktop/TUI 外部调用者同步更新，不留旧路径 facade。 |
| qaqh-gate | 5,171 | provider 请求/流解析、能力配置、模型协议适配 | 不承担会话生命周期；按 provider 拆模块而非三份新 crate；删除按 URL/模型名猜能力的特判；runtime 修复先压测。 |
| qaqh-config | 4,213 | 配置读取/写入、profile 合成、secrets、外部配置导入 | 模块分离持久化/合成/secrets；不新增功能。解析 DTO 不触发会话运行。 |
| sbx-win | 4,087 | Windows sandbox 底层能力 | 保留独立平台边界，ProjFS/ACL 分模块；不与权限策略混并。 |
| qaqh-tool-core | 3,712 | TypedTool、描述符、schema、调用/输出/错误/进度契约 | 解绑 qaqh-skills 具体实现，可信 effect 采用底层契约、宿主应用；旧 ToolResult 往返投影终结在单一消费边界。支持登记契约，授权执行仍归 workspace。 |
| qaqh-mcp | 3,545 | MCP 连接、发现、资源和工具适配 | 登记 API 改依赖 tool-core，消除 workspace 生产依赖；全局 manager 改构造/句柄注入。 |
| qaqh-subagent | 3,455 | 子代理/任务板工具 schema 和宿主端口 | 调度恢复留 runtime，团队存储留 session；工具实现不发 Ringing wire 命令；登记改 tool-core，删 workspace 依赖，lib.rs 按行为拆模块。 |
| qaqh-types | 3,397 | 无环境副作用的共用数据契约 | image_store 磁盘 IO 归 session blob；platform 数据根与环境读取归 platform；tokenizer 初始化移上下文/计数模块并取消 types 默认重依赖；DTO 转换归所属边界。纯 hash/序列化辅助按实际使用保留。 |
| qaqh-lsp | 2,523 | LSP 连接、文档同步、诊断与工具适配 | 登记改 tool-core，删 workspace 生产依赖；会话/取消/manager 显式注入。 |
| qaqh-domain | 2,232 | 与传输独立的业务命令、被动数据记录 | 删 DomainEvent 三套词汇及桥接；本轮保留稳定命令/被动记录，不做纯改名。真正没人需要的 crate 仅在迁移后证实才合并。 |
| qaqh-spy | 2,075 | 工作区变化扫描、审计数据与恢复访问 | shell 副作用扫描保留；绑定显式工作区，审计 journal 与会话事实日志分别说明，不因都叫 journal 合并。 |
| qaqh-skills | 1,983 | 技能解析、资源与激活状态处理 | 状态随会话显式绑定；SDK 不依赖技能实现；删除未用的薄壳/旧入口须先核对消费者。 |
| qaqh-permission | 1,919 | 权限准入、可信目录与冲突判断 | 拆除 current session/workspace/cancel 全局/TLS 和 resolver 钩子；不接管会话生命周期。 |
| qaqh-ringing | 1,643 | 现有 Ringing v2 wire 契约、cursor 编码 | 生产者迁移完成后删除旧 worker wire 与事件转换；保留已使用的命令契约，不改外部协议语义。 |
| qaqh-fs-core | 1,242 | 路径/文件状态/缓存基础能力 | 工作区状态显式实例化；不能反向借 permission 环境状态获得 session；辅助下沉必须有真实共用调用。 |
| qaqh-sandbox | 1,187 | 跨平台执行限制与平台后端选择 | 保留策略与 OS 能力边界；不吞入权限审批。 |
| sbx-cli | 969 | sandbox CLI 诊断/试运行 | 保留独立可执行入口，按命令分模块即可。 |
| qaqh-policy | 627 | 权限/沙箱策略值和纯规则 | 保持无会话环境与 IO；取消 token 不放 policy。 |
| qaqh-config-api | 491 | 配置公共 API 契约 | 保留，避免消费方依赖配置持久化实现。 |
| qaqh-git | 249 | Git 查询/面板操作 | 保留现有职责；去除 workspace 旧别名，不因小而并回工具大包。 |
| qaqh-title | 180 | 标题生成 | 审核 session 依赖是否仅为加载/保存：若是，由调用方供消息并保存标题；否则在任务中记录具体需求。 |
| sbx-nt | 171 | NT 底层定义/调用 | 保留平台底层，不做无收益合并。 |

## 4. 新 crate 的准入与依赖目标

本轮只规划两个有明确边界收益的新 crate，实施前仍须列出迁移符号和调用者：

* qaqh-session-api：抽取 client/daemon/runtime 都使用的 fact/projection/标识契约。
  允许依赖 types/domain 和序列化，不含文件系统、锁、writer、replay IO、tokio。
  session 依赖它，client 不再依赖 session。禁止将全部 session 模块原样搬进去。
* qaqh-platform：数据根定位/验证、环境与平台路径 bootstrap。输入数据根随后显式传下去。
  不拥有图片/blob、会话事实、权限策略、tokenizer。平台存储实现不能再塞回 types。

其余大 crate 优先按职责拆内部模块。新增第三个 crate 必须说明独立所有者、至少一个实际
跨边界消费需求、依赖方向和旧代码删除项；大小本身不是理由。

箭头统一表示“依赖”：

```text
daemon -> runtime -> session -> message -> types
client -> ringing / session-api / types
session -> session-api -> domain / types
daemon -> platform; 其他 IO 宿主按需依赖 platform 并注入路径
工具实现 -> tool-core -> policy / types
workspace -> 工具实现 / tool-core / permission / sandbox / fs-core
runtime -> workspace / subagent / gate
daemon -> 传输控制模块 -> ringing / session-api
```

依赖图允许 runtime 用 session 定义的抽象提交端口；禁止 session 依赖 runtime/daemon。
subagent/MCP/LSP 的工具登记不能为了 ToolManager 依赖 workspace。
迁移后删除过期 path dependency、default feature、shim 和无用第三方依赖。
避免把生产依赖改成 dev-dependency 后就宣称测试边界也已清理。

## 5. 存储权威表

这是现有已核对的核心文件与目标归属；它不是全部数据文件的完整普查。
CLEAN-3 必须补齐实际路径、写入者、崩溃窗口及其他持久文件后才能验收。

| 数据 | 当前事实 | cutover 后职责/恢复来源 |
|---|---|---|
| events.jsonl + commit marker/identity | canonical 事实，生产面未覆盖完整消息 | 会话持久权威；只暴露已提交前缀 |
| 会话 blobs/ | 完整持久正文能力尚未接通 | 会话持久权威；fact 引用须先持久化；无 TTL |
| messages.jsonl | 当前实际消息归档与恢复输入 | 可删除上下文/归档投影，来源为事实+blob；不能继续直写为另一事实源 |
| messages.wal | 消息旧写入/回放 | 删除，不另外造新 WAL 并存 |
| meta.json / index.jsonl | 部分元信息与回合数量参与恢复 | 会话派生字段归投影；配置/运行态逐字段登记，不能假设全文件可从已有事实恢复 |
| timeline snapshot/journal | 展示存储，缺失时从消息/meta 补偿 | 可重建展示投影；来自真实事实，删推算 ID/统一 Completed 补偿 |
| ContentStore | TTL/容量有界展示正文缓存 | wire 缓存；未命中读持久 blob，fact 不引用易失缓存 |
| images/ | types 全局数据根图片存储 | 正文迁至 session blob；旧图片经一次性导入保留引用 |
| recovery intent / tool ledger | 崩溃恢复计划与已执行判定 | 事实决定恢复动作；不确定执行不能伪装成功或盲目重放 |
| device/lease/driver/命令回执 | 控制面持久/运行状态混合 | daemon 所有；设备登记是独立控制面权威，lease/residency 是运行态；命令业务终态由事实或同步 ack 决定 |
| team/task/board | session 内已有独立存储与投影 | CLEAN-3 逐项核对写入/事实覆盖，明确导入及重建来源后再删副本 |
| 文件操作 journal/spy 审计 | 文件 undo/变化记录 | 工作区操作与诊断存储；保留真实 undo 责任，不与 messages.wal 混淆 |
| config/secrets/data-root marker | 用户配置/密钥与数据根所有权 | 独立配置及 bootstrap 权威；不属于会话消息缓存 |

## 6. 源码证据与未证实项

已读源码：types/image_store.rs、platform.rs、token.rs；workspace/lib.rs、execution.rs、
dashboard.rs、tool_side_fold.rs；tool-core/tool_api/context.rs、output.rs；runtime/actor.rs、
registry.rs、service.rs、ringing/v2.rs、timeline.rs、ringing/timeline_rebuild.rs；
session/canonical/store.rs、recovery_executor.rs；subagent/lib.rs 和相关 Cargo manifests。

这些证据确认职责混装与旧导出存在，未证明每个模块都可直接无损删除。
源码审计尚未全覆盖 29 个 crate；编译成本、并发瓶颈与重构后收益未实测。
runtime 的 memory feature 在 manifest 明确为 compatibility no-op，应连消费者一起删除。
tool-core 的 Skill effect 依赖、process-tools 对 file-tools 的辅助依赖需逐符号核对，
不能只按 Cargo 边删除实际行为。

本轮未修改 Rust 行为代码，也未做运行态回归。基线用于后续验证，不能替代回归结果。
