# 当前能力保留矩阵

校准：2026-10-20。本文是迁移输入和验收去向，不是功能完成率、测试通过报告或另一个派工单。
发布规则见 [rollout](rollout.md)，目标所有者见 [架构状态](../ARCHITECTURE.md)。
每次施工先比较本表基线与实际产品提交，更新相关能力行；源码、fixture 和测试存在不等于本轮运行通过。

## 1. 源码与工作树基线

| 仓库/对象 | 核对提交或状态 | 消费/使用边界 |
|---|---|---|
| backend 已提交源码 | `37167d74b96fdef0700f5ca886475fb59e76d238`，`2.0.0-RC2`，32 workspace members | 本文档施工从此提交创建 linked worktree；仍为 Ringing v2 与旧 runtime/消息写链 |
| 原指定目录 | `worktree-v2.1-qaqh-backend`，`8008bccd4e488a171cc516e60c437a16edf22836` | 核对时是拥有独立 `.git` 的 clone，不是原仓的 linked worktree；不以目录名称判断基线 |
| desktop | `6b865949c792a1d8a120c1d3c33f1e7bb41f454f`，核对时干净 | `src-tauri/Cargo.toml` 的 client/types/memwatch 指向同级 `qaqh-backend`；修改另一个目录不会自动进入该构建 |
| TUI | `f7e47af88332986036ce29f97105b659b360a496`，核对时干净 | Cargo path 消费同级 backend 的 client/config-api；账号交互已是实际消费者 |
| Android | `cf63363143e27dffcd8dc26eb264f6ac53a947b0`，核对时干净 | Kotlin `data/ringing/RingingApi.kt`、`RingingConnection.kt`，独立 HTTP/SSE 消费者 |
| Harmony | `901b9d0d741ffa3571364e2ebf753ab6c23c2fd6`，核对时干净 | ArkTS `commons/protocol`、`commons/client/RingingClient.ets` 等已有实现；smoke NAPI 不等于正式 UI 经 Rust core |
| backend 并行未提交施工 | `Cargo.toml`、`Cargo.lock`、`docs/ARCHITECTURE.md` 修改；`crates/qaqh-import/`、导入 spec/ADR 未跟踪 | 外部 transcript 导入未计入上述提交；本轮未复制、提交或验证其代码 |
| 历史设计/审计 | again `13d1e59`、beta4 `6a0e282` 为原文引用 | 本轮未重新核验其 checkout；不能替代当前产品，也不能虚构未入库的 baseline JSON |

这些提交是源码快照，不表示当前机器所运行二进制的来源。需要发布/运行态验收时另核对构建版本、sidecar 与数据根。
跨仓定位使用仓库名和相对路径；本机绝对路径及其他 checkout 的脏文件不成为可移植构建依赖。

## 2. 产品能力、证据与迁移去向

| 能力 | 当前源码证据 | 目标所有者 | 不退化验收 |
|---|---|---|---|
| 四种模型 wire 与显式凭据来源 | [provider 类型](../../../crates/qaqh-types/src/provider.rs)、[配置应用](../../../crates/qaqh-config/src/dto.rs) | config-api/config + provider-api/providers | wire/credential/endpoint 独立；读写接受规范名与已支持别名；非法组合明确拒绝 |
| 命名配置与会话选择（部分实现） | [Config::for_session/apply_profile](../../../crates/qaqh-config/src/config.rs)、[session.set_profile](../../../crates/qaqh-runtime/src/service.rs)、[spawn/reload](../../../crates/qaqh-runtime/src/agent/engine_session.rs) | config repository/resolver + actor selection facts | 保留命名 profiles 与定向选择；修复 credential 拷贝缺口，补资源 CAS/不可变 revision/缺失失败；默认不改变已有绑定，见 [configuration](configuration.md) |
| 配置的全用途/多端消费（存在缺口） | [child 配置](../../../crates/qaqh-runtime/src/agent/spawn.rs)、[工具入口全局读](../../../crates/qaqh-subagent/src/lib.rs)；desktop composer/model-config.ts 与 session/store.ts | 完整请求 snapshot + session effective view | child 不重读全局；账号固定主体；HUD/模型/分母读本会话快照；main/compact/title 保留统一 provider_for，不以 UI profile 名证明实际请求已正确隔离 |
| ChatGPT 订阅账号 | [siwc](../../../crates/qaqh-siwc/src/lib.rs)、[请求构造](../../../crates/qaqh-runtime/src/agent/turn_lap/gate.rs) | providers/account + config credential port + application | 登录/取消/状态/profile/目录/退出；PKCE/回环与代际；管理员授权；token 不进入公开 DTO |
| DeepSeek 账号 | [dsaccount](../../../crates/qaqh-dsaccount/src/lib.rs)、[客户端 DTO](../../../crates/qaqh-client/src/service_types.rs) | 同上，不归 daemon access | 余额/赠额/确认/目录/主动 verify/退出；临时网络失败不误清凭据；Chat 与 Responses 均可选，Messages/Gemini 拒绝 |
| 套餐缓存亲和 | [Responses 请求选项](../../../crates/qaqh-gate/src/responses_sdk.rs)、[session 头](../../../crates/qaqh-runtime/src/agent/turn_lap/gate.rs) | providers/request，actor 提供稳定会话与请求身份 | 套餐路由保留 prompt_cache_key 与 session/thread 亲和头；禁止把亲和 ID 当幂等执行键；不承诺远端必命中 |
| 账号客户端服务面 | [18 项白名单](../../../crates/qaqh-client/src/service.rs)、[方法授权](../../../crates/qaqh-runtime/src/ringing/service_methods.rs) | sdk typed use cases + application + api | 零会话登录可用；保留业务错误分类；白名单/服务实现/管理员约束双向对齐；新路由消费齐备再退出旧出口 |
| 官方 Search/Fetch | [限权端口](../../../crates/qaqh-tool-core/src/tool_api/official_web.rs)、[runtime adapter](../../../crates/qaqh-runtime/src/agent/official_web.rs)、[web tools](../../../crates/qaqh-workspace/src/web.rs) | tool-api port + providers adapter + builtin-tools + executor/catalog | DeepSeek 账号与显式开关同时成立；关闭不额外暴露 search；官方/本地 fetch 路径、错误、限额、保存结果明确；凭据不交模型 |
| 流式调用身份和无法执行的调用 | [gate 事件](../../../crates/qaqh-gate/src/types.rs)、[lap 消费](../../../crates/qaqh-runtime/src/agent/turn_lap/gate.rs) | providers/stream + agent + context | 稳定 part identity、UTF-8 字节水位、call/result 关联、正文与调用共存；无法归一的调用可见失败，不造执行 intent |
| 模型参数容错与别名 | [lenient](../../../crates/qaqh-tool-core/src/tool_api/lenient.rs)、[edit 参数](../../../crates/qaqh-file-tools/src/edit/handler.rs)、[schema 回归](../../../crates/qaqh-runtime/tests/tool_surface_schema_shape.rs) | tool-api 边界 + 各工具 | 保留 type-array/union、字符串整数、单串到单元素数组、file_path/old_string/new_string 与既有别名；拒绝越界和非法类型，不猜逗号分隔 |
| 写路径提取与同文件串行 | [冲突检测](../../../crates/qaqh-permission/src/conflict.rs)、[权限路径](../../../crates/qaqh-permission/src/permission.rs) | workspace resource extraction + executor scheduling + policy | path/file_path/file/paths 在授权和冲突检测同义；同文件双 edit 按模型顺序执行；独立文件仍可并行 |
| 文件修改展示与审计 | [file mutation](../../../crates/qaqh-file-tools/src/file_mutate.rs)、[code delta](../../../crates/qaqh-file-tools/src/code_delta.rs)、[workspace 执行](../../../crates/qaqh-workspace/src/execution.rs) | file-tools + workspace journal | 新旧字段都产生正确展示/行数/审计；估算与真实结果分开；不能把必要写入失败变成 best-effort 成功 |
| 压缩结果、重复压缩与保留区 | [compact engine](../../../crates/qaqh-runtime/src/agent/plugins/engine_compact.rs)、[CompactionEffect](../../../crates/qaqh-message/src/store.rs) | agent-features/compact + context + session-store | 按真实 turn 计算；无新内容不生成摘要、不推进 revision/消息 ID；旧摘要不重复扩增；空摘要失败；超大尾部不清空必要 turn |
| 压缩并发与 UI 原因 | [revision 屏障](../../../crates/qaqh-runtime/src/agent/plugins/engine_compact.rs)、[CompactFinishedView](../../../crates/qaqh-runtime/src/ringing/v2.rs)；desktop `src/session/compact.ts` | feature completion + actor + protocol/sdk | 旧 context revision 的结果丢弃并有终态；未发起/失败/跳过/取消可辨；进度为 replace，投递失败与服务端终态分开 |
| 重启后请求稳定性 | [Chat wire 捕获](../../../crates/qaqh-runtime/tests/restart_request_body.rs)、[Responses wire 捕获](../../../crates/qaqh-runtime/tests/restart_request_body_responses.rs) | context/request snapshot + providers | 同配置/能力/协议版本下重启可重现请求体；追加输入不重排旧输入/工具；缩减工具清单的反例能改变请求；这不是线上缓存命中证明 |
| 实时与最终 usage | [UsageSampled](../../../crates/qaqh-domain/src/timeline.rs)、[瞬态策略](../../../crates/qaqh-runtime/src/ringing/persistence_policy.rs)；desktop `src/session/store.ts` | providers → session live；最终值 committed；diagnostics 可旁路采样 | 缺 prompt 不清零，cache 未报告区别于零；采样不重置 tok/s/流窗口或关块；不落永久日志、不重复累计最终 usage |
| 多端连接、配对与审批 | [Rust client](../../../crates/qaqh-client/src/client.rs)、[daemon 路由](../../../crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs)；各端路径见上表 | access + sdk/subscriptions + application/api | 保留设备 token/scope/吊销、重连、历史/实时一致和审批；移除 lease/driver 前覆盖 Rust、TS、Kotlin、ArkTS 消费者 |
| 桌面 GenUI v0.1/v0.2 | [backend prompt](../../../crates/qaqh-runtime/src/agent/prompts/backend_prompt.md)；desktop `src/genui`、`src/genui/v2` | client renderer；原文归会话内容 | 版本分流、流式与终稿一致、局部 state、安全目录、错误回退；replay 不自动 followUp/执行；不是 Ringing major 3 |
| 平台执行与 workspace 审计 | [sandbox](../../../crates/qaqh-sandbox/src/lib.rs)、[workspace](../../../crates/qaqh-workspace/src/lib.rs) | policy/executor + workspace/terminal + OS adapters | 保留当前准入、路径和审计算法；真实平台另验 kill+wait/隔离/网络；代码存在不证明新终端接管已实现 |
| MCP/LSP/skills | [MCP](../../../crates/qaqh-mcp/src/lib.rs)、[LSP](../../../crates/qaqh-lsp/src/lib.rs)、[skills](../../../crates/qaqh-skills/src/lib.rs) | 注入 managers + catalog + feature reducer + extensions | 保留已有连接/解析/工具能力与 lock 依赖；逐步替换全局 manager，不将已有配置导入称作完整安装服务 |

矩阵记录仍有效的产品行为，不把现行实现缺陷升级为目标契约。尤其 canonical 摘要引用、写入错误传播、
全局 manager、双写和日志/投影完整性仍须按目标修复，不能因为已有新业务便宣称旧结构问题已经消失。
configuration 的新增结论是静态源码审查；未运行跨凭据切换/并发保存/多端真机复现，不能将其写成已通过的回归。

## 3. 并行导入功能的协调边界

业务 checkout 当前有外部会话导入草案与未提交 crate；它们未进入本工作树和已提交能力行。
后续纳入时分别报告原始字节保真、消息/分支保真、上下文可恢复性，不用“备份 JSONL”替代语义验收。

- 外部源发现、稳定捕获、来源图、规范化与覆盖报告归独立 import 模块；不读取进程全局环境，不运行源文件中的命令。
- 正式 session/blob/facts/catalog 仍由 application + session-store 管；来源解析器无第二个会话 writer。
- 共享身份/内容模型按 kernel/context/session-api 归属，不把并行方案暂用的 types 当终态所有者。
- 原 QAQH 格式迁移仍按 staging/停写/切根规则；外部导入可以是新会话创建用例，不要求停止所有无关会话。
- 只读 imported transcript 与 runnable checkpoint 明确区分；不继承源审批、工具执行、PTY、账号、Hooks 或缓存。

独立 import crate 的最终模块、允许边和数据契约待该切片确认后同步更新 dependencies.json；当前 46 包图不包含它。

## 4. 切片验收与本轮验证范围

每个切片至少明确：涉及哪些能力行、真实调用者、输入/输出契约、错误/取消/恢复、旧路径退出与实际验证结果。
删除旧测试时，相关仍有效性质迁到新所有者；不能只保留 beta4 样本或以新类型编译通过代替产品验证。

| 验证层 | 本轮范围/后续要求 |
|---|---|
| 源码 | 已核对 Git 基线、依赖路径、上述生产路径和测试样本；未全面审计全部 crate/客户端 |
| 结构 | 2026-10-20：validate 通过（46 包）；含多配置专题的 links 通过（179 个本地文件链接）；4 个结构工具反例测试已通过；check 返回 1、88 项差距（32 个实际包对照 46 个目标），未达到终态；命令见 [audit 工具](../../../tools/v2-architecture/README.md) |
| Rust/多端产品 | 本轮未运行 cargo fmt/clippy/workspace tests、客户端类型检查/构建或真机回归；文档与只读工具检查不构成产品通过 |
| 远端账号/平台 | 本轮未操作真实账号、联网模型服务、PTY/沙箱或安装第三方包；fixtures 不证明服务当前可用 |
| 尚缺配套 | 原 baseline JSON、完整安装 JSON Schema、major 3/ClientTool 跨语言消费向量未随原设计入库；不得宣称这些检查已具备或通过 |
