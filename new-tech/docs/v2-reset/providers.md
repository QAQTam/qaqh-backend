# Provider、账号与模型能力

状态：2026-10-20 校准的目标契约；当前已提交源码为 `37167d7`。API key / ChatGPT 订阅 / DeepSeek 账号已有实现，目标所有权尚未迁移；证据见 [preservation](preservation.md)。
本文件定义能力去向，不表示新的 Rust ABI、资源路由或凭据迁移已经完成。发布按 [rollout](rollout.md)。

## 1. 不合并的边界

| 维度 | 含义 | 目标所有者 |
|---|---|---|
| wire | Chat Completions / Responses / Messages / Gemini 的模型协议 | provider-api / providers |
| credential source | api_key / chatgpt_plan / deepseek_account；显式选择，不根据空 key 猜 | config-api + config binding |
| provider account | 模型网站登录、OAuth/PKCE 回环、refresh/verify/revoke、状态/目录 | providers/account，凭据经 config 的受限端口持久化 |
| daemon access | principal/device/grant/配对/吊销 | access，不接管模型网站账号 |
| UI | 系统浏览器授权、轮询状态、取消/退出、账号和模型展示 | desktop/SDK，永不读取 token |
| application | 管理员准入、typed 账号用例、配置关联、目录查询与错误映射 | application；api 仅映射传输 |

providers 通过注入的凭据端口使用 config，不能为方便而让 config 反向依赖具体 OAuth adapter。
Agent 只接收固定的 provider request/config/capability snapshot；不承担登录、文件存储或全局账号选择。
命名 profile、ModelBindingVersion、默认/会话选择、各用途快照及当前 credential 复制缺口见 [configuration](configuration.md)。
请求所用账号是确切 binding，不由 manager 的“当前账号”在每次请求重新决定；同主体 token 刷新与改选账号分开。
现有 `qaqh-siwc` / `qaqh-dsaccount` 能力映射进目标模块，不要求永久保留这两个 crate，也不得因为原图漏列而删除业务。

## 2. 必须保留的产品语义

- **API key**：保持 endpoint / wire / credential source 的独立配置与现有四种协议；密钥读模型保持掩码。
- **ChatGPT 订阅**：状态、profile 列表/选择、登录 start/status/cancel、退出、模型目录；保留 state/nonce/PKCE、回环约束、ID token 校验、refresh/撤销及主机身份规则。
- **DeepSeek 账号**：登录 start/status/cancel、状态、余额/赠金、已展示赠额确认、模型目录、主动凭据校验与退出；attempt id 防止旧页面取消新登录。
- **账号路由**：chatgpt_plan 只配 Responses 与绑定官方 origin；deepseek_account 配 Chat 或 Responses 与绑定推理 origin，Messages/Gemini 拒绝。保存期验证 wire，请求期再验 endpoint 和凭据绑定，不退回旧的 Chat-only 限制。
- **配置拼写**：wire 读模型使用 chat/responses/messages/gemini；写侧接受当前支持的规范名及历史别名并归一，未知值拒绝。新 config-api 与客户端不得重建“读 chat、只能写 openai”的错配；旧 daemon 所需转换有明确版本范围。
- **错误分类**：远端暂不可用/网络失败不等于未登录；只有明确的失效证据才清凭据。状态缓存不能证明凭据有效，主动校验不能在断网时伪报失效。
- **退出**：本地凭据删除与远端撤销分别表示；撤销失败不能偷偷恢复本地登录或无限重试。旧 refresh/登录 completion 不得覆盖已退出/切换后的代际。
- **目录与推理配置**：模型可用范围、上下文/输出预算、reasoning effort（包括现有 ultra 档）、thinking 类别、usage/缓存信息沿原能力保留；不要把不同 provider 的枚举强行等同。

DeepSeek 账号与 ChatGPT 订阅不是两种 daemon 设备身份；不得将其 token 当 device grant 或 ClientToolHost token。
公共配置/facts 只记录 opaque credential binding/account revision，不放访问 token、refresh token、授权码或明文 API key。
原账号存储加密/权限约束必须保持；迁移失败具名报错，不静默回落到其他 endpoint、账号、BYOK key 或付费额度。

## 3. 安全与生命周期

现行 `siwc.*` / `dsaccount.*` 是管理员服务面；资源化后仍须逐方法授权，不能因路由拆分让配对设备获得本机登录/切换账户权。
renderer 只收到脱敏投影和授权 URL，由原生受限入口打开系统浏览器；token 不进入 webview、URL 查询、日志、facts 或诊断。

目标 manager 是 daemon 构造并注入的实例，持有取消、超时、关闭与 join 所有权；原 global manager slot 不作为新契约保留。
登录 attempt/account/refresh generation 显式区分；取消/退出之后的迟到回调不重建授权。每次推理必须复核当前凭据有效性与绑定 origin。
凭据刷新可更新 token generation，但不能暗中变更当前 session 选择的账号/endpoint；配置与安全吊销按各自所有者生效。

当前 Rust client 已有 8 项 siwc、10 项 dsaccount 的受限服务出口和 typed DTO，TUI 是实际消费者。
账号操作在零业务会话时可用；登录不能要求先 attach 一个空 session。白名单、实际方法与 admin 授权双向核对，
保留 transport/API 错误与业务失败的区分。目标资源化在所有消费者迁移后退出该出口，规则见 rollout。

## 4. 流与工具调用不退化

保留当前 gate 修复：

1. 工具调用身份不能仅靠流中可变/缺失的 call ID；稳定 part key 与最终 call ID 关联，参数按实际字节水位追加，不能重复累计全文或截坏 UTF-8。
2. 模型同时输出正文与工具调用时保留非空 assistant content；thinking 类别与 provider 私有 bundle/签名不得因展示拆分而丢失。
3. 无法完成归一化的上游调用在 UI 中给出明确失败，不能无声消失、假装执行，或产生可被恢复器重放的虚假 intent。
4. main/compact/title/subagent 的请求身份、预算与用途显式传入；request metadata 只按 endpoint 声明能力发送，不能按 URL/模型名猜。
5. 保留 provider URL/path 归一化、工具调用/结果配对、兼容预算与流 usage；不能从旧版 gate 再搬回已修过的行为。
6. ChatGPT 套餐请求仍发送 prompt_cache_key 与稳定会话亲和头；不能把被禁的 prompt_cache_retention 推广成整族缓存字段禁用。亲和身份与 request/attempt/command 身份分开，不用会话头承诺执行去重。
7. 同配置、能力快照与序列化版本下，恢复后保持模型输入、工具顺序和静态请求参数稳定；新增一轮只追加对应上下文。缓存测试需到真实出站 HTTP 请求体，不只比较内存 Message；协议升级允许显式版本边界，不承诺远端一定命中缓存。

展示 item 与模型 bundle 的依赖闭合遵循 [messages](messages.md)；模型参数边界容错遵循 [engineering](engineering.md)。
第三方 SDK 版本仅是当前实现证据，不冻结为终态依赖版本；升级必须跑关联 ID、参数、预算和 provider 反例。

## 5. 官方网页工具的独立端口

当前 `official_web_tools` 默认关闭；启用要求 DeepSeek 账号凭据来源与显式能力开关同时成立。
search 是 executor 执行的普通工具，web_fetch 可用官方或本地 HTTP 后端；二者不同于 provider 自行运行的 hosted web_search，
也不是扩展包安装的 MCP。开关关闭不额外暴露 search，不因发现账号就修改工具清单。

| 所有者 | 责任 |
|---|---|
| config-api/config | 保存显式能力与凭据绑定，随 turn/request 固定选择；当前吊销仍即时复核 |
| tool-api | OfficialWebTools/结果与具名错误的中立端口，不依赖具体账号 crate |
| providers | 官方 Search/Fetch HTTP 适配、正确 origin 的凭据解析、限制和错误分类；daemon 注入端口 |
| builtin-tools + tool-catalog | canonical search/web_fetch、输入输出投影、获准能力与路由快照 |
| executor + workspace | 网络/文件资源准入、取消、限额；output 保存走受管文件写入与 journal |

当前端口的 String 错误不作为新 ABI 的先例；目标使用中立具名错误，不把 DsError 或 token 透传工具层。
现行未装配后端时 web_fetch 可走本地抓取；已选官方后端的请求失败则明确失败，不自动重发到另一后端或换凭据。
迁移须在能力描述中声明允许的降级、实际执行位置及错误；本地直接抓取与官方远端抓取的资源边界分别校验。
必要保存/journal/blob 写入失败必须传播，不能沿用当前部分保存路径的静默失败并宣称成功。

## 6. 切片验收

- 每种凭据来源：主请求/compact/title/subagent 按实际支持范围验收，不把一个成功主回合当成所有路径完成。
- 登录：成功、取消、过期、旧 attempt 回调、profile 切换、refresh 并发、退出后迟到 completion；本机重启后状态一致。
- 远端：凭据失效与临时故障分流、校验成功/失效/断网、目录/余额失败的 typed/degraded 展示、退出撤销失败及有限重试。
- 安全：非管理员被拒、跨 endpoint 不发 token、renderer/公共 DTO/错误与日志无 secret、迁移中凭据存储失败不报成功。
- 消息：正文 + tool calls、交错/变动 call ID、累计/增量参数、多字节切片、tool result 配对、丢弃调用可见失败、thinking/effort/usage 保真。
- 跨仓：账号设置和模型切换 UI、TS 导出、sidecar 版本组合、历史与实时渲染一致。
- 请求：DeepSeek Chat/Responses 两路配置与鉴权，套餐亲和键/头，重启后的真实请求体稳定性和工具清单改变反例。
- 官方工具：开关关闭/启用、账号未就绪、权限拒绝、远端失败、结果限额、取消与保存失败；不按 URL/模型名猜后端，不将 fetch 降级扩为模型凭据回退。

已有源码/fixtures/测试不表示本轮已通过验证；真实账号网络探针需独立授权和可用凭据，离线 fixture 不能证明远端当前可用。
