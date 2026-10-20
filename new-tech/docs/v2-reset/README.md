# QAQH v2.1 → v3.0 重构入口

校准日期：2026-10-20。历史方案来自 `v2.0-again`，不再将其 alpha10/30-crate checkout 描述为当前工作树。
当前源码起点为后端 `37167d7` / `2.0.0-RC2` / 32 members；多端提交、真实依赖与未提交施工见 [能力保留矩阵](preservation.md)。
原 `v2.0.0-beta4` / `6a0e282` 仅是历史审计基线，main 不再冻结，不能从 beta4 直接重写并漏掉后续变动。

**保留当前产品能力与成熟算法；从 v2.1 起随功能切片重构发布，到 v3.0 完成架构收敛。**
本目录定义目标，不声称已实现。业务线继续更新，当前源码仍含旧 runtime/存储链。只改变内部所有者时保留现行产品契约；
新 wire / 新存储的破坏性切换必须独立验收，不与产品 v2.1 或 GenUI v0.2 的版本号绑定。
发布节奏、临时桥和旧路径退出统一遵循 [rollout](rollout.md)，能力不退化以 [preservation](preservation.md) 为准。
扩展生态与 ACP 目标由 ecosystem.md/acp.md 补充；当前安装服务尚未实施。
历史 bug、交接、ADR、审计和旧设计从冻结 tag 查看，不在当前 docs 继续维护。

施工依据：

- [渐进发布](rollout.md)：功能切片、版本边界、数据切换和发布门槛。
- [能力保留](preservation.md)：已核对产品增量、迁移去向与接缝验收。
- [多配置与 config/config-api](configuration.md)：当前链路、成熟实现研究、命名资源/CAS、默认/会话/请求快照及全用途继承。
- [Provider 与账号](providers.md)：账号登录/校验/退出、凭据隔离、模型目录和推理能力。
- [Ringing 协议](protocol.md)：事实消费、增量、诊断、终端和恢复。
- [消息、撤回与 fork](messages.md)：独立内容单元、序号、可运行边界和历史操作。
- [扩展生态](ecosystem.md)：MCP/LSP/skills、市场兼容、安装/更新/卸载与能力快照。
- [组合与热插拔](composition.md)：借鉴 Cordis 的资源作用域，固定执行代际、依赖与 shutdown 证据。
- [客户端工具与 SDK](client-tools.md)：UI/工具宿主/daemon、离线故障、执行许可和多语言消费。
- [ACP 双向适配](acp.md)：编辑器接入、跨 agent 委派与执行主机边界。
- [薄 loop 与功能插件](agent.md)：状态所有权、effect、工具和存储路径。
- [实现简化与测试取舍](engineering.md)：防御逻辑的边界、旧机制退出、测试规模样本与施工标准。
- [依赖矩阵](dependencies.json)：v3.0 的 46 个目标 crate、模块和允许依赖。
- [结构闸门](../../../tools/v2-architecture/README.md)：validate 检查目标图，check 检查实际 workspace，links 检查文档文件链接。
- 历史 beta4 度量仅保留 engineering.md 中的取样说明；原 baseline.json 未入本仓，不虚构恢复。

## 1. 架构裁决

部署模型是模块化服务器 + 独立 UI 客户端 + 可选 ClientToolHost。daemon 拥有 loop、模型、会话存储和自身 workspace。
客户端工具通过固定宿主路由提供本机资源，结果经 actor 提交；UI 与执行宿主分开。
桌面 Cowork 让多个 UI 连接同一 daemon/工具宿主；首期 PTY 仍在 daemon，作业不随 UI 迁移。
引入有明确身份与故障语义的客户端执行，不拆微服务；详细契约见 client-tools.md。

当前错配来自拆分不完整：UI 偏好仍在服务端读模型；租约混合身份、附着、授权和 driver；
MessageStore/WAL 与 canonical 分别写入；agent 与 wire 耦合；插件依赖巨大共享状态。
当前 theme 写路径已部分本地化，Ringing v2 已有 fact_seq/cursor。重构解决统一所有者和恢复语义，不以“完全没有 seq”作为前提。

## 2. 状态唯一所有者

| 状态 | 所有者 | 消费方式 |
|---|---|---|
| 主题、字体、语言、通知、草稿 | 每端 PreferencesStore | 本地读写，离线立即生效 |
| host 配置、profile、secret 引用 | config repository | 版本发布、CAS、下一 turn 生效 |
| principal/device/grant/吊销 | access | 稳定身份，与连接独立 |
| 会话 facts、正文、命令回执、交互结论 | session-store | actor 单一提交端口，可重放 |
| turn、待执行 effect、取消树 | agent actor | 事实恢复，supervisor 收割任务 |
| AgentFeature 持久状态 | 会话 facts 的 namespaced 投影 | 核心提交，功能插件无独立 writer |
| 扩展安装、scope binding、安装 operation | extensions registry | 独立持久 workflow；turn 固定能力快照 |
| 文件 journal 和实际 undo 正文 | workspace | 文件操作权威，会话引用结果 |
| PTY、进程、输入 writer、接管 fence | terminal | 实例句柄，生命周期结论进入 facts |
| 诊断队列、采样、保留窗口 | diagnostics | 可关闭、可丢，不控制业务 |
| 客户端实际工具/资源/执行回执 | ClientToolHost | 本机权限与作业 journal，结果报 daemon 提交 |
| 网络队列、received/applied cursor | subscriptions/sdk | 断开释放，授权和执行继续 |

## 3. 配置目标边界

四个模型为 ClientPreferences、HostConfig、ProfileVersion、SessionSelection。
Preferences 不经过 daemon。HostConfig 管理服务、存储和执行上限；ProfileVersion 管理模型、endpoint、凭据引用及能力。
发布 profile 产生不可变 revision；SessionSelection 通过 facts 选择确切 revision 和允许覆盖。
完整契约见 [configuration](configuration.md)：ProfileVersion 引用完整 ModelBindingVersion，
NewSessionDefaults 只提供新建推荐；既有 session 与在飞 request 不跟随全局 active_profile。
保留现有命名 profile/session.set_profile，不把当前 API 的扁平 DTO 和 credential 复制遗漏当作新设计。

每 turn 固定 TurnConfigSnapshot，不同 session 不共享 active profile。
默认 profile 更新只影响新会话；已存在会话的选择变更要显式命令、CAS、下一 turn 标记。
host 安全上限收紧对下次工具执行立即生效；旧快照不能绕过吊销。
secret 使用专用 resolver，不入 facts，不回落到另一个 endpoint 的 key。
先持久 secret 版本，再原子发布配置及 current 指针，禁止生效配置引用不存在的 secret。
账号型 provider 不是普通 api_key 的别名：保留 ChatGPT 订阅和 DeepSeek 的登录、状态、校验、退出与目录能力，
按 [providers](providers.md) 分离 provider account / config credential binding / access device identity。

## 4. 存储裁决

blob 持久化 → facts batch + commit marker 持久化 → 投影 → 发布。
每会话只有 session-store writer。facts 不含累计流式文本；ContentRef 必须存在且可取回。
完整 provider 消息、签名、工具参数/结果/顺序、多模态正文都必须能重放。
transcript 按独立 thinking/text/tool_call/tool_result item 展示，保留原始 response bundle 作为模型投影依据。
消息身份、服务器展示序号、事实序号分开；撤回 append facts、不回退序号；fork 不重放副作用，见 messages.md。
摘要先 put_blob 再写 compact fact，不能把 hash 当未落盘指针。

context fold 是纯变换；具体 tokenizer 归 providers，context 只接 TokenCounter port。
新存储切片完成后删除该路径的 messages.jsonl/WAL/PersistOp/SaveFull、独立 timeline writer、从 meta 猜 Completed 的读取补偿。
在此之前未迁移的会话继续使用当前写链；禁止为达到目标图提前拔除必要写入，禁止同一会话两个权威 writer。
context/timeline/meta/team/诊断索引都是可重建投影；读取不能唤醒业务或补造成功。

catalog 独立持有创建/删除工作流：先 intent，再会话数据操作，再 terminal；崩溃可对账，list 只显激活成员。
重构不自动删除旧用户数据。旧格式仅离线 import，在 staging 根验证后切换；
无法证明上下文保真时只导入只读 transcript，不允许猜测式 resume。
这里的旧格式迁移不同于 Claude Code/Codex 等外部会话导入。后者的来源解析可由独立 import crate 拥有，
application 编排、session-store 提交和 catalog 激活；原始保真、消息保真与可继续执行分别验收。

## 5. crate 责任

| 包组 | 责任 |
|---|---|
| kernel / policy / config-api | 最小标识和引用、纯规则、配置契约 |
| context / session-api / session-store | 纯消息 fold、事实/端口、唯一持久 writer |
| agent-api / agent / agent-features | 插件/effect 契约、薄状态机/邮箱、compact/技能/子代理 |
| provider-api / providers | 请求/账号端口契约、API/账号型供应商适配、登录与校验、模型目录/流解析 |
| tool-api / tool-executor | 工具契约、准入与执行 |
| workspace / terminal | 文件资源/undo、PTY/接管/进程生命周期 |
| file-tools / process-tools / builtin-tools / git | 具体能力与显式宿主端口 |
| extension-api / extension-formats / extensions | 扩展契约、公共/vendor 格式、安装 registry/workflow |
| tool-catalog / acp-client / acp-server | 能力快照、外部 agent 委派、编辑器前门 |
| skills / mcp / lsp | 目录及协议 manager，构造注入、shutdown/join |
| config / access | 配置/secret、身份/授权 |
| protocol / subscriptions / application / api | 公共 DTO、重放/发送、用例、HTTP/SSE/WS 映射 |
| diagnostics / memwatch / host | 采集/索引、内存 sampler、数据根/锁/发现 |
| sdk / client-local | 公开协议消费、本地发现/托管 |
| client-tools / tool-host | 服务端远端 ToolRunner、可选 Rust 客户端宿主 |
| sandbox / sbx-nt / sbx-win / sbx-cli | OS 强制、平台底层、验证入口 |
| daemon | 唯一 composition root 与生命周期监督 |

当前确认的图共 46 包；完整模块和生产/dev/build 允许边在 dependencies.json。数量不是成果指标。
并行 import 提案待契约及实际消费者锁定后更新图，不为维持 46 而塞回 runtime/types；详见能力矩阵。
禁止万能 types/event bus/service locator，禁止把 runtime 改名成 agent 大包。
agent 不依赖 wire、HTTP SDK、具体存储/工具/PTY/诊断或 agent-features；daemon 注入插件和实现。

types/domain/ringing 分发到所有者后删除；gate 改 providers；message 的纯 fold 归 context；
session 拆契约/存储；runtime 拆 agent/features/application/subscriptions/api/access/diagnostics/daemon；
permission 规则归 policy、审批执行归 executor；fs-core/spy 实例资源归 workspace；
subagent 编排归 agent-features、工具入口归 builtin-tools；title 归 application 后台用例。
新增 siwc/dsaccount 的账号协议、refresh/verify、目录与推理适配归 providers；凭据绑定归 config、业务用例归 application。
二者不归 access，不因 46 包原图未列出便删除其能力；原全局 manager 改由 daemon 注入实例。
保留 patch/query/provider parser/sandbox/MCP/LSP/git 成熟算法，去掉全局配置/TLS/取消表/隐式 fallback runtime。

## 6. 技术依赖与施工出口

下表是切片间的技术依赖，不是先完成 A–E 才能发布的串行大工程；实际发布规则见 [rollout](rollout.md)。

| 顺序 | 交付 | 删除旧路径 |
|---|---|---|
| A 契约/真实消费者 | 事实/消息/工具/插件/协议契约，SDK 纯 DTO 依赖 | client→session/types，旧 facade re-export |
| B 唯一存储/薄 loop | commit/recovery、真实邮箱、插件效果、完整重放 | WAL/PersistOp、镜像 TurnActor、DomainEvent 回转、共享巨型状态 |
| C 执行/交互 | executor/terminal、接管 facts、诊断有界采集 | TLS/global process/permission、第二份 pending 权威 |
| D 服务/多端 | Config/Access、新 Ringing、SDK reducer、本地偏好、客户端工具固定路由与回执对账 | lease/open/renew/attach/driver、catch-all RPC、旧路由 |
| E 切换 | 离线导入、平台、恢复/取消/性能验收 | 双 writer、兼容开关、runtime/ringing 旧包 |

在功能边界内允许整段替换；每次从最新产品基线迁移并补齐业务增量，不再维护“不追补丁的冻结线”。
临时单向 adapter 必须有所有者、兼容范围、退出条件与最迟移除版本，不能成为永久 facade，不引入新的权威双写。
每项迁移按 [工程规则](engineering.md) 消除旧补偿的前提，并同步处理专属测试；保留现行边界与必要接缝。
验收覆盖完整重放、故障不提前发布、快照/追流无漏、重启 cursor、审批 CAS、PTY fence/断连不重发、
kill+wait、诊断不影响 turn、配置隔离/离线 UI、导入不伪造成功、平台能力诚实。
外部副作用不承诺 exactly-once，按 PureReplay/Reconcile/NonReplayable 分类，证据不足为 Indeterminate。

结构命令（仓库根，Python 3.11+）：`python tools/v2-architecture/audit.py validate`、`links`、`check`；最终目标 check 必须通过。
当前源码为 32 个 workspace 成员；check 是 v3.0 全量终态检查，
中间版本按切片核对新增依赖及明确未迁移范围，不能把预期全量失败当成功，也不能因此阻止全部功能发布。
Rust 落地后运行对应接缝和 fmt/clippy/workspace test；本轮设计不以未运行测试充当实现完成证据。
