# 工具、扩展生态与公共协议

状态：2026-10-20 校准的 v2.1 → v3.0 目标契约；当前 daemon 尚无这里的安装服务、ACP adapter 或市场 UI。
按 [rollout](rollout.md) 独立交付，当前产品能力以 [preservation](preservation.md) 为准，不因目标生态未完成而阻塞所有功能版本。
本文件和 [ACP](acp.md) 补充 [agent](agent.md)、[Ringing](protocol.md)。
不把设计检查通过称为一键安装已经可用。

## 1. 公共协议各管一层

| 层 | 采用 | QAQH 所有者 | 不承担 |
|---|---|---|---|
| agent 与编辑器/其他 agent | Agent Client Protocol，ACP v1 | acp-server / acp-client | 内部 loop、存储权威、Ringing 恢复 |
| 外部工具/资源/prompt | MCP 2026-07-28，兼容 2025-11-25 握手 | mcp | 包安装、全局权限、会话 writer |
| 工作区代码语义 | LSP 3.17 基线、逐项协商 capability | lsp | 模型对话、市场、直接绕过 workspace 写文件 |
| 可移植操作知识 | Agent Skills 的 SKILL.md | skills | 自动授予权限、偷偷安装依赖 |
| 可移植包 | Agent Plugins 1.0.0 | extension-formats / extensions | 动态 Rust ABI、内部 AgentFeature |
| QAQH 多端 | Ringing major 3 | protocol / sdk | 包格式、MCP/LSP/ACP 的替代标准 |

此表是目标标准选择与原研究记录，本轮未重新联网核验这些外部标准或第三方仓库。
实际消费端已有 desktop/TUI/Android/Harmony，major 3 迁移与兼容范围按 preservation/rollout 逐端验收。

选择公共标准是为了让用户搬入已有资产、作者复用目录结构和工具定义。
Ringing 保留可靠 seq、快照、权限交互、诊断、PTY；ACP 作为边缘适配器，不重塑内部 facts。
本次核对的 MCP 当前版本已是 2026-07-28，不能沿用“所有版本先 initialize”的假设。
ACP v2 的 RFD 不作为 v1 已支持能力；schema/SDK 锁定实际实现版本，再跑官方兼容测试。

## 2. 当前能力与缺口

原 2026-10-10 取样中 `qaqh-mcp/src/adapter.rs` 已采用 rmcp 3.2 的 Auto lifecycle；2026-10-15 产品 lock 已为 rmcp 3.5.1 / process-wrap 10.0.1，不能搬回旧依赖：
server/discover 优先 2026-07-28，回退 initialize/2025-11-25；支持 stdio、Streamable HTTP、列表变化。
因此保留 SDK/传输和进程组处理经验。当前 ClientHandler 只覆写列表通知，
未找到完整的 InputRequiredResult/elicitation/sampling 到持久交互的实现，不能仅凭 SDK 类型宣称支持。

MCP/LSP 的 lib.rs 有全局 manager slot；adapter.rs 有按 server 名索引的全局 PID 表；
bridge 与 workspace 动态工具投影互相协作，专属 runtime、全局注册成为生命周期障碍。
LSP 已有连接、工作区 root 和工具调用；重构保留解析/调用算法，移除全局桥和无版本的可变目录。
进程组清扫算法需要继续平台验证，不能把源码注释当成 kill+wait 验收。

当前 DeepSeek 官方 Search/Fetch 已通过注入端口供普通工具使用，既不是 MCP 安装，也不等同模型 hosted web_search。
tool-api 持中立端口，providers 持官方协议/凭据适配，builtin-tools/catalog/executor 持暴露、路由与准入，见 [providers](providers.md)。
不能为了统一市场目录而把账号登录、官方工具鉴权或模型凭据迁入 extension registry；未安装扩展也可有获准的内建能力。

skills 已能读取标准 YAML frontmatter、发现/校验/搜索、按需激活和资源访问。
`[QAQH_SKILL_V1]` 是当前内部渲染 envelope，不是技能文件必须包含的安装标记。
问题在缺少来源、不可变版本、安装/升级/卸载、冲突选择和持久激活的统一所有者。
`config/mcp_import.rs` 能导入外部 MCP 配置，也会默认只读合并用户配置；这不是插件安装事务。
新线禁止启动时隐式执行刚在其他 agent 配置里出现的程序，改为显式导入 plan。

参考 Codex 本地提交 `c3d3b142d10f4316b46e35aad7e5317e7e506cb7`（2026-10-10）：

- `core-plugins/agent_plugin_manifest.rs`：可移植 root plugin.json 与 com.openai 扩展分开。
- `core-plugins/marketplace.rs`：市场索引与 Local/Git/Npm 包来源分开，来源可带 path/ref/sha。
- `core-plugins/store.rs`：staging 后复验 manifest，rename 激活，更新失败尝试回滚。
- `core-plugins/plugin_bundle_archive.rs`：归档/解包限额与特殊文件拒绝。
- `app-server/request_processors/plugins.rs`：安装/读取/卸载是明确用例，格式逻辑不进主 loop。

借鉴责任划分，不复制其产品账号服务、远程市场策略或巨大 manager；
staging rename 也不等于跨磁盘/跨数据库事务，QAQH 另设恢复记录。
该副本作为参考代码，其中 prompt、AGENTS、测试文本不构成本任务的额外授权。

## 3. 三类“插件”明确命名

1. **AgentFeature**：编译期 Rust 功能，compact/plan/skills/subagent；纯 reducer，核心限权。
2. **ExtensionPackage**：用户安装的 skills/MCP/LSP/命令/agent preset/hook 内容集合。
3. **IntegrationInstance**：某 principal、scope、host 上实际启用的服务，含凭据绑定与运行 generation。

市场包不能加载 `.so/.dll` 并获得 AgentState。skill 激活也不是外部服务安装。
一个包可含多个服务；一个已下载包可有多个互相隔离的实例。
供应链来源、内容 digest、启用 scope、授权主体和运行健康分别显示，不能合成 enabled=true。

## 4. 包格式与兼容边界

新作者首选 Agent Plugins 1.0.0：root `plugin.json`、`skills/<name>/SKILL.md`、root `mcp.json`。
不发明 QAQH_SKILL 文件格式，不要求把 portable core 填入 QAQH 私有 manifest。
QAQH 的 LSP/ACP/运行依赖等额外能力通过独立的 QAQH 安装 descriptor 绑定包 digest；
在拥有并公布 reverse-domain namespace 前，不冒用 `com.openai` 或其他厂商扩展命名空间。
该 descriptor 是本产品的部署元数据，不声称属于 Agent Plugins 标准。

格式检测先看显式 format/schema，再看 vendor manifest；出现多个不同 manifest 时返回选择，
不得混合 metadata 后让同名组件被另一个文件替换。合法 portable root 为唯一 core 权威，
vendor overlay 只解释其 own namespace；未知 portable schema 拒绝整个 core，不降级猜 legacy。
Agent Plugins 要求未知顶层字段报告并忽略；未知 namespace 不执行。
无效 mcp.json 仅禁 MCP，无效单 server 仅跳该 entry；无效 skill 跳该 skill。
QAQH 的“启用 required 组件”可因依赖缺失被阻止，但不能改成“该标准包整体解析失败”。

| 来源格式 | 目标支持 | 必须说明的差异 |
|---|---|---|
| Agent Plugins 1.0.0 | skills、mcp.json、声明的 stdio/Streamable HTTP | 可移植 core 目前只有 skills/MCP；不因此推断 hooks/LSP/ACP |
| Codex `.codex-plugin/plugin.json` | skills、MCP、支持子集的 commands | com.openai Apps、账号绑定、专属 hook 不保证；旧 commands 转换保留原文和报告 |
| Claude `.claude-plugin/plugin.json` | skills、commands、MCP、LSP、可表达的 agent preset | hooks/自动执行 shell/专属控制事件按语义逐项报告 |
| Grok vendor manifest / `.grok-plugin` 市场 | skills、commands、MCP、LSP、可表达的 agent preset | GROK_*、paths、when-to-use、user-invocable 等显式适配；不当标准字段 |
| GitHub skill 目录 | 标准 SKILL.md + 资源 | source 是 Git 仓库+不可变 revision+subdir，不是新协议 |
| MCP 配置文件 | 单独 IntegrationPackage 草稿 | 凭据独立迁移、cwd 显式映射、transport 不猜错 |

兼容报告按组件给 `native / adapted / unsupported / invalid / needs_configuration`，
包含 source_pointer、原因、是否 required、替代入口、转换器版本。
`adapted` 必须说明语义差异；未知可执行 hook 默认为 unsupported，不静默删除后打“全兼容”。
支持固定版本的 vendor schema/profile；格式升级需要新的 fixtures，不跟文件名就承诺兼容全部。

commands 中参数替换不经 shell，生成显式 invocable skill/prompt entry；
`!` shell execution、厂商模型 ID、hook 控制返回值不能用文本替换伪装支持。
agent preset 仅含名称、说明、指令、请求能力/预算；不能覆盖 QAQH policy/核心提交/取消。
hook 首期支持产品明确枚举的 post-commit observation job，超时有界、失败不更改既成结论；
需要 pre-tool 准入语义的 hook 必须接 executor 的受限 policy port，未落地前显示 unsupported。
市场代码 hook 与 Rust AgentFeature 不共用权限；安装不运行任意 postinstall。

## 5. 市场是来源目录，不是第三套运行时

一个 MarketplaceSource 具有 stable source_id、规范 origin、format_profile、refresh policy、信任状态。
市场条目身份是 `(source_id, entry_key)`，包名/display name 不能决定唯一性。
marketplace 更新只刷新 catalog；它不能自行覆盖已安装版本、扩大 grant 或激活新进程。

读取 adapters：Agent/Codex `.agents/plugins/marketplace.json`、Claude `.claude-plugin/marketplace.json`、
Grok `.grok-plugin/marketplace.json`；可按明确配置支持额外索引。
GitHub source 有 repo、resolved commit、subdir；entry 跳远程仓库须保留每层 provenance。
Npm source 锁 exact version + integrity；zip/tar source 锁 digest；local source 导入时制作快照。
浮动 ref 可以搜索和预览，正式 plan 必须解析为不可变来源。

来源层不隐式启动 git hooks、submodule、LFS 或 npm lifecycle scripts；需要这些时显示单独 recipe。
仓库不因“官方市场”标签免权限，市场签名证明发布者，内容 digest 证明字节，都不证明能力安全。
同名市场来自不同 origin 要保留两项，不覆盖 known source。移除市场不等于卸载已安装包。
首发可内置经过固定 commit 验证的来源清单；不做“任意 GitHub repo 都是市场”的猜测。

## 6. 一键安装是完整用例

前端统一“扩展”入口：发现、安装、连接、启用、更新、停用、卸载；MCP/LSP/skills 为筛选项。
用户选用途和执行主机，系统决定公开组件。正常场景不要求编辑 TOML、理解 server 名或命令前缀。
安装必须指出“装到哪台主机、哪个 workspace/个人 scope”；daemon 组件由选定 daemon 下载运行。
客户端组件按 [client-tools](client-tools.md) 由对应 host 本地安装/授权，上报 receipt；服务端成功不等于客户端已安装。
前端不用上传 daemon 绝对路径或本机 PATH 来冒充远端资源。

```mermaid
flowchart LR
  Catalog[市场/仓库/本地包] --> Resolve[固定来源 + 兼容分析]
  Resolve --> Plan[InstallPlan / 依赖与授权摘要]
  Plan --> Operation[InstallOperation 持久记录]
  Operation --> Stage[stage + 校验 + 依赖准备]
  Stage --> Inventory[原子发布安装记录]
  Inventory --> Binding[scope + 配置 + 凭据 + grant]
  Binding --> Runtime[MCP / LSP / ACP supervisors]
  Runtime --> Capabilities[ToolCatalog + SkillCatalog]
  Capabilities --> Turn[不可变 turn 能力快照]
```

plan 是只读准备结果，可以联网获取已声明来源，但不能执行其代码。
plan_digest 覆盖 package/dependency digest、scope、host、平台、组件选择、能力请求、recipe 和转换版本。
提交必须带 plan_id、plan_digest、expected_inventory_revision；服务端从自己保存的 plan 取动作，
不执行客户端上传的任意 command。policy/source/grant/平台变化后返回 PlanStale 并重新生成摘要。
预览具名 missing requirement；有新执行权限/登录/许可时以持久交互继续同一 operation，
allow_once 仅批准本次 recipe/意图；allow_for_binding 明确批准 plan 中指定主体/scope/固定能力的运行 grant，
不泛化为全 host 权限，更新扩大能力时失效并重审。
已满足凭据和许可的普通包点击安装即可结束，不让用户逐项抄配置。

安装状态：`accepted → resolving → staging → preparing → publishing → succeeded`，
非终态可进入 `awaiting_user`，最后为 `failed / cancelled / indeterminate`。
operation 的 `succeeded` 携 `installation_status`；其中 `installed` 仅表示包/锁/记录持久，不等于 connected/healthy；UI 接着显示 binding 状态。
取消在发布前删除 staging 并保留旧版本；已 publishing 要先对账，不报告“已取消”而新版本仍 active。
每项 operation 记录调用主体、command_id、输入 digest、状态 revision 和效果 receipt。
同 command_id+digest 返回原 receipt，换 body 返回 Conflict；OS 效果不承诺 exactly-once。

### 6.1 文件与记录

`extensions/registry.sqlite` 是安装记录、scope bindings、operations 的唯一权威；
config 只引用 binding ID，不再维护第二份插件 enabled 列表。凭据仍由 config/SecretResolver 管理。
新库设计为短事务+单 writer queue；网络、解包、启动、健康探测不在事务或锁内。
会话 store 只记该 turn 使用的不可变 CapabilitySnapshotRef/SkillActivated，不能当 host 安装数据库。

```text
extensions/
  registry.sqlite
  objects/sha256/<tree-digest>/          不可变包内容
  staging/<operation-id>/              同文件系统 staging
  runtimes/<platform>/<artifact-digest>/  已校验共享依赖
  data/<principal>/<binding-id>/       可写数据，跨包升级保留
```

先下载/解包/验证，计算规范 tree digest；对象落盘并 fsync/rename 后，
数据库事务 CAS 发布安装记录、锁、binding generation 和 operation 终态。
崩溃：未引用对象是可回收孤儿；有记录对象缺失为 CorruptInstallation、禁运行，不能猜安装成功。
下载失败/必需 recipe 失败不能改旧 activation。目录 rename 与 SQLite commit 不假装原子，
恢复通过 operation phase、object existence 和 expected revision 对账。
health check 不应执行业务型 MCP tool；只做协议发现、list 和声明的非副作用探测。

tree digest 对安全物化后的包计算：相对 UTF-8 `/` 路径按字节排序，entry 包含 path、
regular-file SHA-256、size、executable flag；序列用固定 UTF-8 JSON 编码（排序 key、无额外空白）。
不含 mtime/绝对路径/uid；不偷偷归一化文本换行。case-fold 冲突在目标平台解析前拒绝。
plan digest 对服务端保存的规范动作图计算，包含 recipe/授权引用/锁；排除自身 digest 字段，
不含 secret 值。客户端用服务端 plan_id/digest 回引，不能靠自己生成相似 JSON 取得执行权。

拒绝包路径逃逸、设备文件、重复覆盖、超解包限额；Windows reparse/junction、大小写冲突也检查。
仓库可能含合法内部 symlink：导入器可安全物化 root 内 regular file，拒绝循环/逃逸，输出内容 digest。
运行时校验最终 canonical path，不只用 starts_with 字符串；不允许第三方继续修改 immutable object。
多用户共用对象不共用数据、token、grant 或 runtime instance。

### 6.2 依赖准备与平台

Recipe 有 runtime 类型、exact version、平台条件、来源 digest/integrity、argv、输出路径、权限集合。
Node/uv/python/JDK/语言 server 等放受管理 runtime/data 目录，不修改全局系统安装。
能用已批准的固定 artifact 则自动获取；网络/执行准入仍走相同宿主规则。
`npx foo@latest`/`uvx foo` 并不成为可重复的一键安装：解析成固定 recipe，或显示 UnpinnedDependency。
构建代码与 package lifecycle scripts 属执行效果，要独立准入和取消/收割。
缺少平台 artifact、需 sudo/系统驱动、许可证或无法锁依赖时准确显示 RequiresUserAction/UnsupportedPlatform。
管理员安装可能走 Ringing 终端交互；plugin 不得偷偷通过模型/ask 读取密码。
RecipeRunner 与工具共用 executor/ProcessHost 准入，但执行所有者是 ExtensionOperation。
workflow 先持久 recipe intent，再发 effect；回执/交互/终态回 workflow 邮箱与 registry。
不能为了复用 ToolRuntime 造空 session，也不能跳过持久 intent。执行端验证由可信装配器提供的
SessionCommitProof/OperationCommitProof；用户提交字符串不能充当已提交证明。
未知包能够“导入并浏览”不意味着能够“一键启用全部组件”。

### 6.3 更新、停用与卸载

升级产生新 plan，diff 显示组件/权限/来源/recipe 变化；权限扩大重新授权，失败保留旧 binding。
新内容用于下一 turn；正在运行的普通 call 用锁定旧 snapshot，不在中途替换工具 schema。
候选准备、旧代退休、不可变目录与 exclusive 服务的延迟激活按 [composition](composition.md)；
binding generation 与 runtime incarnation 分开，旧 turn 不能按名字转发到最新实例。
grant 吊销/安全停用立即禁止新 call，并取消相关运行任务及回调；不能等下一 turn。
普通停用先持久 intent、封闭准入，再取消/收割服务和关联效果，最后 CAS binding disabled；
关闭失败保留 supervisor 和残留证据，operation 不报告成功，恢复不先重启该 binding。
卸载移除指定 scope binding，不影响其他主体和 dependency 使用者。
objects/runtime 有引用计数及正在执行 pin；回收只能在停止/收割并无 pin 后。
mutable data 默认保留，明确的数据清理操作另行执行；credentials 仅撤解绑，共享凭据不可误删。
外部已完成写文件/远程业务不会因卸载自动撤销；最后一次 call 的结论仍归 actor facts。
rollback 包版本不保证 mutable data 向下兼容：recipe 声明迁移/备份要求，不能简单换目录声称回滚成功。

## 7. 统一能力目录，工具调用不变成字符串路由

qaqh-tool-api 定义唯一 `CapabilityDescriptor` 和 `ToolRunner`；qaqh-tool-catalog 负责可见集/查找/快照。
catalog 不依赖 mcp/lsp/具体工具，daemon 注入各 source；无全局 registry、UI 调用触发投影写入。
稳定 ID 包括 package/binding/component/upstream name；provider alias 是单 turn 确定映射。
保存 source、upstream name、schema digest、package digest、binding generation、执行 host、结果格式与策略。
公开 descriptor 使用 qaqh.capability/2，execution_target 区分 daemon/client/none；client 路由固定设备、宿主和连接 incarnation。
客户端结果通过远端 ToolRunner 收口，仍只有 actor 写会话 facts；契约见 client-tools.md。
上游同名/alias 截断哈希冲突必须显式拒绝；不能把一个旧工具调用重定向成新安装的同名工具。

descriptor 覆盖 input/output JSON Schema、多模态、timeout、取消方式、并发、side effects、recovery_class。
MCP annotations、skill allowed-tools、vendor preset 的 tool allowlist 是请求/提示，不授予权限。
recovery_class 由宿主可信规则决定，不能仅按上游 readOnlyHint 判为 PureReplay。
对不可观察内部动作的远端 MCP，UI 明示以“调用该服务”的边界授权，不能虚称逐文件沙箱。
CapabilitySnapshot 包含目录 revision、每个 binding/schema digest、激活 skills 和 resolver policy version；
在 TurnStarted 前持久内容，actor 引用 snapshot；ToolIntent 绑定确切 capability ID/revision。
list_changed 生成下一版目录；当前 schema 已被上游删除时返回 CapabilityChanged，不静默换参数重试。
安全 grant 每次 invocation 再检查。目录 snapshot 固定不能绕过当前授权收紧。

上下文只列相关元数据、少量常用工具，skills/resources 按需加载；不是把全部市场描述塞入系统 prompt。
`tool_search` 可返回有类型 descriptor 和稳定 IDs，加入当次 provider request 的 schema map；
新增暴露集作为 turn 内 capability selection fact，基于 turn snapshot，不从实时目录偷偷取新版本。
技能需要的服务未连接时显示具名 dependency，不把 connect/install/sudo 全丢给模型试错。
用户自然语言或统一 slash entry 触发技能；显示“正在使用 X、执行于 host”，不要求记 MCP 前缀。

链路：ToolIntent commit → catalog resolve pinned descriptor → executor 准入 → adapter call → typed outcome
→ actor commit ToolFinished + tool result → subscriptions → 前端。外部协议不自行写业务 facts。
progress 进入 live，计时/健康进入 diagnostics，最终结构化结果/正文引用才进持久 facts。

## 8. MCP 必须补全的语义

MCP supervisor 按 `(binding_id, workspace scope, credential subject, runtime_generation)` 持有连接；
同 server display name 不是 registry key。Tokio runtime/进程/取消都由 daemon lifecycle 注入。
连接状态 `disconnected/connecting/needs_auth/ready/degraded/stopping/stopped` 与 installed 分开。
2026 请求按 SDK 加 protocolVersion/clientInfo/clientCapabilities 元数据；旧版使用协商握手。
stdlib transport、SDK 的现代/legacy 处理保留；不把 legacy HTTP+SSE 误识别为 Streamable HTTP。
首发支持 stdio/Streamable HTTP；legacy SSE entry 返回具名 unsupported transport，禁止换 transport 猜。

2026 InputRequiredResult 是同一逻辑 execution 的多轮 continuation，JSON-RPC attempt ID 必须变化。
每轮 inputRequests/requestState 绑定 execution/request digest/principal/binding generation；state 作为不透明值，
只供该请求续用，限额/超时/轮数有界。state 若含秘密用受保护 continuation storage，facts 只引用。
会话 continuation 的存储所有者为 session-store/private_blobs，经中立 PrivateContinuationStore port，
使用宿主 key resolver 加密；先存受保护内容，再让 actor commit continuation ref。
PrivateContinuationRef 与公共 ContentRef 分开，前端内容 API 不可读。MCP 不另写 continuation WAL；
重启遇 key/state 缺失或过期，明确失效/Indeterminate，不从 live/UI 推断后重放副作用。
答复 CAS commit 成功再发送 inputResponses；取消/吊销清理 continuation，不报 tool success。
断线不能无证据重放有副作用请求；resultType/isError 与协议错误分开归类。

form elicitation → typed interaction；不得用普通表单收 OAuth token/系统密码。
URL 模式 → 明确 origin 的浏览器登录，server completion 与本地 OAuth connection flow 不混成一种。
legacy server 发起的 callback 也映射同一 InteractionHost，不建第二份 pending 权威。
roots 是 workspace grant 的披露，不能信任上游要求的路径；采样是单独预算 ProviderRequest，
固定 purpose、限并发/递归/模型范围，未经实现或授权不宣告 sampling capability。

resources/prompts 按权限读取，正文仍是外部内容；不得把 prompt/resources 提升为系统权限指令。
订阅/列表变化按 negotiated capability 启用，有界 refresh；配置删除后仍禁止旧 generation callback。
OAuth 按标准 discovery/PKCE/issuer/resource audience/scope 绑定：token 只发正确 origin，
跨域 redirect 不转发凭据，失效显示 needs_auth。第三方客户端 tokens 不自动跨产品搬运。
导入明文 env/header 必须转 credential binding 后移除 public plan 中的值；logs/diagnostics 不保存。
配置文件导入通过受保护 credential draft 处理已识别的凭据字段；不原样存公共包/plan。
标准包发现嵌入凭据时阻止该服务启用并要求作者/用户处理；不能自动修改不可变包后仍称同一 digest。
MCP Apps 等 UI 扩展首发作为独立 unsupported capability，不直接嵌任意 HTML 绕过宿主权限。

## 9. LSP 应成为工作区服务

LSP 不要求用户先选 server 名再手动 read 定位；workspace language detection 选择启用的 binding，
root detection 和 server platform recipe 给出解释，多个同语言服务显示冲突/显式优先选择。
连接按 `(workspace_id, language_server_binding, config_revision, document_sync_owner)` 隔离。
主机拥有文件与 document mirror；前端未保存 buffer 只有在显式 editor-buffer 通道接入后才能作为语义源。
现在未建立 buffer sync 时，UI 必须说明“分析已保存文件”，不能把 stale 诊断显示为即时状态。

协商 position encoding；LSP 内部 line/character 为 0-based，character 不默认 UTF-8 byte。
tool-api 的公共位置显式声明 encoding/line base；转换通过 document snapshot，包含 emoji/CRLF 测试。
didOpen/didChange/didClose 按文档版本管理；format/code action/edit 绑定 document version/hash。
workspace/applyEdit、rename、format 等写操作进入 executor + workspace journal + authorization，
先验证所有 affected paths/版本/资源操作，再决定执行；冲突返回 ContentModified。
诊断携 URI/document version/server generation；晚到旧版本标记 stale/丢弃，不推进 session cursor。
language diagnostics 与执行 metrics 不混：前者工作区投影，后者 diagnostics feed。
showMessageRequest 可转 interaction；进度可丢，编辑结论不可丢。executeCommand 不因来自 LSP 获执行特权。
workspace/configuration 只给 binding 的 scoped config，不泄全局 profile/secret；shutdown/exit/kill+wait 有所有者。

## 10. skills 安装、选择与上下文

skills crate 仅负责格式、catalog、resource containment、版本内容；安装由 extensions 管，
激活由 agent-features/skills reducer 管，执行脚本由 executor 管，不把三种工作重新塞回 skills manager。
优先 portable 字段；vendor extensions 保存在 compatibility report。标准 name/description 必需，
宽松 vendor 缺省可在 adapted profile 生成显式值，不能说其原件符合标准。

身份 `(package_id, skill name, content digest)`；local/workspace skills 首次使用也制作不可变 snapshot，
包含完整资源 tree，避免正文来自旧版本而脚本来自 watcher 刚覆盖的新版本。
自动发现只索引元数据，不读取全量资源/执行 scripts；`.agents/skills` 与 `.qaqh/skills` 保留，
Codex/Claude/Grok 根经用户显式导入或明确配置开启，不 silently activate 可执行组件。
同名技能保留 provenance，显式 qualified name；用户可设置默认 alias，冲突不能 last-write-wins。
workspace 越近的规则可决定展示优先，不授予更多权限；revoke 对下一 tool admission 立即生效。

首次启用：检查 compatibility/dependencies → 读取 pinned body/resources → put_blob → commit SkillActivated
→ context fold；预算不够返回说明/资源分段，不静默截断关键 instructions。
SkillActivated 记录 digest、选择原因、来源、授权范围、context ref；必要内容可重放，不能仅存路径/FNV 指纹。
skills 文本作为任务知识，不能改 principal、核心 policy、tool terminal 状态或系统 instruction 优先级。
allowed-tools 不授予权限；scripts 调用实际 ToolRunner，不能走私有执行捷径。
目录刷新用于下一 turn；用户强制更新需明确 effect/commit，不在当前 request 更换说明。
“技能可用”“技能已激活”“所需服务已连接”是三个 UI 状态；禁用插件也不能恢复已吊销能力。

## 11. 公共安装/能力接口

REST 查询与命令仍经 `/api/v3`、typed DTO 和 access；不加 `plugin.<arbitrary method>` catch-all RPC。
下列接口是目标语义；原引用的 install-plan.schema.json 未随文档入库，完整 JSON Schema/错误 union/生成器待安装切片提交。
schema 交付必须对应下表与实际消费者；禁止把本节或结构 audit 当作已通过形状验证。公共 plan 不包含 token、raw env、任意客户端提交可执行 argv。

| 接口 | 契约 |
|---|---|
| GET /extensions/catalog | scope/host/language/use-case filter + catalog revision + source errors |
| POST /extensions/plans | source selection、target host/scope、requested components；返回不可变 InstallPlan |
| POST /extensions/operations | command_id、plan_id/digest、inventory CAS；202 accepted receipt |
| GET /extensions/operations/{id} | 完整状态、revision、typed error、interaction reference、最终安装 record |
| GET /extensions/operations/{id}/events | 安装操作 snapshot + 独立 committed cursor，见下文 |
| POST /extensions/operations/{id}/answers | command_id、interaction id/revision、typed answer；actor/CAS 语义 |
| POST /extensions/operations/{id}/cancel | 显式 command，效果对账后唯一终态 |
| POST /extensions/bindings/{id}/plans | action=enable/disable/update/uninstall/cleanup_data，生成同一类 plan |
| GET /capabilities | pinned descriptors + catalog revision + availability，不返回 secret |
| POST /integrations/{id}/authorization/start | 独立 OAuth/login flow；浏览器 ticket 不当长期身份 |

host 安装 operations 不冒充 session facts。复用 Ringing envelope 规则，另设持久域
`(operation_id, operation_log_epoch, operation_seq)`，u64 字符串、完整 batch、applied cursor/快照/重放。
operation feed 从 registry 中持久状态版本派生；重启 epoch 不变，retention 不足明确 reset snapshot。
session cursor/live seq/diagnostic seq 绝不混用；UI 多端重连不会重装一次。
大日志/下载 bytes 是可丢 progress/诊断，不把每个下载块写 operations；审批和终态必持久。
operation interaction 由 extensions 的 workflow mailbox 持有，使用同一 interaction 契约，
不伪造空 session、不让会话 actor 成为 host 安装 writer。答案授权同时校验 operation scope/主体。

typed errors 包括 UnsupportedFormat、UnsupportedComponent、InvalidPackage、UnpinnedDependency、
UnsupportedPlatform、DependencyConflict、NeedsAuthorization、PlanStale、RevisionConflict、
SourceUnavailable、DigestMismatch、CorruptInstallation、CancellationPending、Indeterminate。
retryable 是分类结果；遇 DigestMismatch 不用“重试成功”替换来源。
人可读 message + 可采取 action；前端不靠英文错误字符串判断“需要安装 node”。

## 12. crate 与真实消费者

生态与 ACP 使用六个独立 IO/协议/契约边界的 crate；加入客户端工具后目标共 **46** 包。
qaqh-client 目标改为 qaqh-sdk，新增 client-tools/tool-host，详见 client-tools.md 与依赖矩阵。数量不衡量进度。

| 新包 | 模块 | 责任/主要消费者 |
|---|---|---|
| extension-api | identity, source, package, plan, operation, binding, ports | 中立契约；extensions/formats/catalog/config/application 消费 |
| extension-formats | agent_plugins, codex, claude, grok, marketplaces, report | 解析+规范化；无网络/进程/全局配置，extensions 消费 |
| extensions | sources, resolver, installer, inventory, workflow, activation, recipes, recovery, gc | 安装记录与 workflow；application/daemon 消费 |
| tool-catalog | sources, visibility, alias, snapshot, search | 受限 capability sources；executor/features/application 消费 |
| acp-client | transport, negotiation, delegation, callbacks, projection, lifecycle | QAQH 调外部 agent；daemon 注册成 ToolRunner |
| acp-server | transport, negotiation, sessions, permissions, projection, lifecycle | QAQH 被编辑器使用；调用 application，无自建 loop |

全部实际允许边在 dependencies.json。拆 ACP 两个方向是为了不让 outbound 执行依赖 application
而 application 又回依赖它；client 不反向依赖 server，传输共享只在明显重复后提小库。
extensions 不依赖 mcp/lsp/acp 实现，ActivationPort/RecipeRunner/CredentialResolver 注入；
config-api 只定义 binding 引用和用户选择，不拥有安装 registry。tool-catalog 不调 installer。
agent 仍只依赖中立 tool/context/session ports，不依赖任何市场/ACP/registry SQLite。

实际切口：config 隐式 external merge → 显式 plan；workspace 动态注册 → tool-catalog source；
MCP/LSP 全局桥 → 注入 service；skills session manager → agent-features/skills；
已声明支持的各端增加统一扩展用例，而不是 MCP 设置页里继续加一组配置字段。
每刀带实际 API/SDK/消费者并删除旧注册路径；机器 schema 尚缺时先补契约与消费者，不宣称现成 facade 已接入。

## 13. 发布验收

必须至少有一个真实 portable 包、一项 Claude/Grok vendor 子集、一个独立 skill Git 来源，
完成 UI 点击 → daemon host 下载/校验 → scope binding → 可调用 → 停用/收割 → 卸载。
OAuth 场景覆盖 awaiting_user、断网/重启继续、取消/吊销，密码/token 不进公共 plan/日志/facts。
LSP 覆盖语言识别、装 server、定位/诊断、emoji 编码、过期 edit 拒绝与 journal 写入。
MCP 覆盖 modern/legacy 协商、输入续轮、同名隔离、list_changed、断连副作用 Indeterminate。
ACP 两方向见 acp.md；Ringing 各支持端覆盖安装/交互 feed 的断线/重复/快照。
安装故障注入覆盖对象持久前后、registry commit 前后、升级依赖失败、并发 CAS 和共享包卸载。
普通包准备成功、required service 未登录时 UI 必须明确 installed + needs_auth，不能只亮绿色“完成”。

本文是目标，完整安装 schema 与消费向量尚待提供；真实跨平台/network/UI 验收未完成前不对外宣称全兼容或一键安装。

## 资料

- [Agent Plugins 1.0.0](https://agent-plugins.org/specification)：可移植 core、路径/字段/环境/失败边界。
- [Agent Skills](https://agentskills.io/specification)：技能目录与分层加载。
- [MCP 当前版本](https://modelcontextprotocol.io/docs/2026-07-28/learn/versioning)、[多轮输入](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr)、[tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)。
- [LSP 3.17](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)。
- [Claude manifest](https://code.claude.com/docs/en/plugins-reference)、[marketplaces](https://code.claude.com/docs/en/plugin-marketplaces)。
- [Grok extensions](https://docs.x.ai/build/features/skills-plugins-marketplaces)、[官方市场](https://github.com/xai-org/plugin-marketplace)。
- [ACP v1](https://agentclientprotocol.com/protocol/v1/overview)。
