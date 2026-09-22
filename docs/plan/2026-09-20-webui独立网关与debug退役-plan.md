# WebUI 独立网关与 `/debug` 退役设计（Phase 1）

> 日期：2026-09-20
> 基线：`betav2 @ 50d3dc1`（PR #175 merge）
> 状态：**Phase 1 实施中**。§0 的方向、§3 的路由/进程边界和 §8 的推荐项已冻结；
> 普通 daemon 默认关闭与独立 `webui` 网关骨架进入实现。
> 读者：daemon / runtime / client 维护者，WebUI 前端负责人，安全与发布负责人。
> 关联报告：
> - `docs/archive/2026-09/report/2026-09-12-timeline持久化死锁与debug桥token泄露-report.md`
> - `docs/buglist/2026-09-16-安全并发与审查登记-buglist.md`
> - `docs/handoff/2026-09-17-buglist复核九批次执行-handoff.md`
> - `docs/spec/2026-09-15-前端契约与client-API稳定性-spec.md`

---

## 0. 结论先行

1. **WebUI 默认关闭，且不是“路由返回 403”，而是根本不挂载浏览器入口。**
2. **普通 `qaqh-daemon run` / `qaqh-daemon server` 不提供 WebUI；只有显式执行
   `qaqh-daemon webui` 才启动浏览器网关。**
3. 推荐把 `webui` 做成**独立、临时、仅回环的本地网关进程**，而不是继续在 daemon
   内部常驻 `/debug`。
4. `/debug` 作为产品入口退役。若仍需开发诊断，使用显式、独立、默认关闭的诊断模式。
5. 浏览器永远不接触 daemon 全权 Bearer；使用短生命周期、HttpOnly、SameSite=Strict
   的浏览器会话。
6. 默认关闭只降低暴露概率，不降低开启后的爆炸半径。CSP、URL 白名单、CSRF、防
   clickjacking、seed/method scope 仍属于合入闸门。

一句话：

> **WebUI 是独立的浏览器控制面，不是 daemon 的调试附属路由；默认不存在，显式启动，
> 临时存活，最小授权。**

---

## 1. 目标与非目标

### 1.1 目标

- 消除 daemon 长期暴露 `/debug`、nonce、token 兑换和静态托管的风险面。
- 为 WebUI 建立独立进程、独立端口、独立浏览器身份。
- 浏览器只持有受限会话，不持有 daemon Bearer token。
- WebUI 关闭后，浏览器攻击面整体消失，daemon 的 Ringing API 不受影响。
- 把 `qaqh-webui` 源码纳入后端仓库时，构建与运行生命周期可审计、可重复。

### 1.2 非目标

- 本期不把 WebUI 作为 LAN 或公网远程管理面。
- 本期不追求浏览器与原生壳的完全同权。
- 本期不允许通过配置文件、安装器或后台服务隐式启用 WebUI。
- 本期不解决 Level 4 `exec` 沙箱问题；该问题仍是独立安全项。

---

## 2. 当前事实与风险

### 2.1 设计启动时事实（Phase 1 已开始改变）

- `run` / `server` 都会挂载 `/debug` 路由：
  `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:161-165`。
- `/debug/__qaqh_bridge__.js` 只下发 nonce：
  `crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs:151-163`。
- `/debug/__qaqh_token__` 兑换后返回 daemon 全权 token：
  `crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs:178-239`。
- `/debug` 当前只有 CORP + nosniff，没有 CSP / frame / COOP / CSRF：
  `crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs:459-476`。
- `is_authorized` 使用普通字符串比较：
  `crates/qaqh-daemon/src/axum_server/axum_impl/auth.rs:5-10`。
- `crates/qaqh-daemon/webui-dist/index.html` 是 `.gitignore` 忽略的本地生成占位，
  由 `crates/qaqh-daemon/build.rs::ensure_webui_embed()` 生成或从 sibling 同步；当前
  仓库不包含可审计的产品 WebUI 实现。
- 临时 WebUI 源码与 `streaming-markdown` 依赖不在本仓库；合入时必须把源码、锁文件和
  构建产物边界一起纳入，不能沿用仓外 sidecar 的隐式信任。
- `/health`、`/activity` 不在 `/debug` 前缀下，不受 `/debug` 回环守卫约束：
  `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:135-136`。

Phase 1 已删除普通 daemon 的 `/debug` 挂载、nonce/token 桥和 `rust-embed`
静态托管；`qaqh-daemon webui` 改为启动独立 `qaqh-webui-gateway`，只保留安全
占位页，等待 Phase 2/3 接入构建产物与受限浏览器会话。

### 2.2 风险判断

历史报告把 `/debug` 视为“调试桥 token 泄漏”。合入 WebUI 后，风险类别变化为：

```text
静态资源托管
  + 浏览器同源执行环境
  + 模型/工具输出渲染
  + 工具审批 UI
  + daemon 全权 Bearer
  = 浏览器控制面风险
```

因此 `/debug` 不应继续按“开发辅助面”定级，而应与 Ringing 控制面同级审计。

---

## 3. 目标架构

### 3.1 进程拓扑

```text
┌───────────────────────────────────────────────────────────┐
│ qaqh-daemon run / server                                  │
│ - Ringing HTTP/SSE + Bearer + lease                       │
│ - 不挂载 /debug、/ui、nonce、静态托管                     │
└──────────────────────────┬────────────────────────────────┘
                           │ local discovery + Bearer
                           │（仅网关进程持有，浏览器不可见）
┌──────────────────────────▼────────────────────────────────┐
│ qaqh-daemon webui                                         │
│ - 独立、临时、仅 127.0.0.1 的浏览器网关                   │
│ - 静态资源 / 浏览器会话 / CSRF / CSP / SSE 代理            │
│ - 浏览器 principal 仅具备受限方法集与 seed scope           │
└──────────────────────────┬────────────────────────────────┘
                           │ same-origin Cookie
┌──────────────────────────▼────────────────────────────────┐
│ Browser WebUI                                             │
│ - 不接触 daemon token                                     │
│ - 不接触 daemon discovery 文件                            │
│ - 只访问网关同源面                                        │
└───────────────────────────────────────────────────────────┘
```

### 3.2 路由边界

网关路由建议：

```text
GET  /                                                  静态 SPA
GET  /assets/*                                          静态资源
GET  /__gateway/bootstrap.js                            一次性 nonce
POST /__gateway/session                                 nonce -> HttpOnly browser session
GET  /__gateway/sessions                                脱敏 session.list
POST /__gateway/sessions/{seed}/attach                  显式 seed attach
POST /__gateway/ringing/commands/{channel}              命令 allowlist
GET  /__gateway/ringing/commands/{id}                   命令回执
GET  /__gateway/ringing/content/{content_id}            内容读取
POST /__gateway/ringing/content                         内容上传
GET  /__gateway/ringing/events/{channel}                三频道 SSE
GET  /__gateway/ringing/sessions/{seed}/bootstrap       bootstrap
GET  /__gateway/ringing/sessions/{seed}/timeline        timeline 快照
GET  /__gateway/ringing/sessions/{seed}/timeline/events timeline SSE
POST /__gateway/ringing/service/{method}                service method allowlist
```

禁止浏览器路由：

```text
/ringing/v1/clients/open
/ringing/v1/leases/renew
/control/v1/*
/debug/*
```

网关不得笼统代理 `/ringing/*`；所有 daemon 端点必须逐条登记。

daemon 路由建议：

```text
普通 run/server：无 /debug、无 /ui、无 __qaqh_bridge__、无 __qaqh_token__
可选诊断模式：显式 --diagnostics，仅回环，独立命名空间，默认关闭
```

### 3.3 生命周期

- `qaqh-daemon webui` 启动独立网关。
- 默认绑定 `127.0.0.1`，端口默认随机；需要固定端口时必须显式传入。
- 网关必须显式获取 daemon discovery 信息；若 daemon 不可达则退出，不静默拉起后台服务。
- 网关退出、Ctrl+C、会话结束或超时后，浏览器请求立即失联。
- 不提供“安装为后台服务”“开机自启”“安装器自动启用 WebUI”的路径。
- 不允许 `run` / `server` 通过普通配置隐式打开 WebUI。

---

## 4. 浏览器身份与会话

### 4.1 不变量

1. 浏览器永远不接收 daemon Bearer token。
2. 浏览器会话必须是不透明随机值，服务端维护映射。
3. Cookie 必须 `HttpOnly`、`SameSite=Strict`、host-only、短 TTL。
4. Cookie 不作用于 `/control/v1/stop` 等 daemon 管理面。
5. 网关重启或 daemon epoch 改变后，浏览器会话失效。
6. SSE 不把 lease/token 放进 URL；浏览器原生 `EventSource` 通过同源 Cookie 鉴权。

### 4.2 建议流程

```text
1. 浏览器加载 /
2. 浏览器加载 /__gateway/bootstrap.js
3. 网关签发一次性 nonce，TTL 短，限制并发与签发速率
4. 浏览器 POST /__gateway/session { nonce }
5. 网关校验 Origin + Host + Sec-Fetch-Site，兑换成浏览器会话
6. 网关 Set-Cookie: HttpOnly; SameSite=Strict; Path=/__gateway
7. 浏览器 fetch / EventSource 携带 Cookie
8. 网关校验 CSRF / Origin，再以 Bearer 访问 daemon
```

### 4.3 能力范围

浏览器 principal 不是全权 token。至少执行以下约束：

| 能力 | 默认策略 |
|---|---|
| 读取当前 seed 的 timeline/bootstrap | 允许 |
| 发送消息、取消当前回合 | 允许，绑定当前 seed |
| 回答 ask / plan / permission | 允许，必须绑定 challenge ID |
| 读取其他 seed | 拒绝 |
| `fs.list` / `fs.read` | 网关强制绑定 active seed cwd；daemon 增加显式作用域并做组件级前缀校验 |
| `config.load` / `config.save` | 默认拒绝或仅返回脱敏读模型 |
| `workspace.delete` / 跨 workspace 操作 | 默认拒绝 |
| `/control/v1/stop` / `stop-if-idle` | 拒绝 |
| 任意方法名透传 | 拒绝；网关使用明确 allowlist |

### 4.4 浏览器 session ↔ daemon lease 映射

每个浏览器 session 独占一个由网关持有的 daemon lease。浏览器只看到网关 Cookie，
看不到 `client_session_id`。

```text
browser_session_id (opaque, cookie)
  -> gateway-owned client_instance_id
  -> daemon client_session_id
  -> 当前唯一 active seed
```

规则：

- 浏览器 session 创建后，网关调用 `POST /ringing/v1/clients/open`。
- 网关在所有命令、service、SSE 代理中同时注入 `Authorization: Bearer` 与
  `x-qaqh-client-session-id`。
- 网关按 daemon 返回的 `renew_interval_ms` 续租；续租失败时关闭该 session 的 SSE，
  并让浏览器重新 bootstrap/session。
- daemon epoch 改变后，旧 lease 与旧 SSE 全部失效。
- 网关退出时清理浏览器 session 映射；daemon 侧 lease 由 TTL 自然过期。
- seed 切换是**重新协商**而不是续租：先关闭旧 seed 的全部 SSE，再以同一
  `client_instance_id` 调 `open` 获取新的 `client_session_id`，旧 lease 的 seed 归属
  由此清空；随后通过唯一审计入口 attach 新 seed，最后才允许新 SSE 建连。
- 网关调 `open` 时**禁止携带 `attach_seed`**；所有 seed 归属只能走
  `POST /__gateway/sessions/{seed}/attach` 对应的显式命令，避免绕过审计。
- URL 中的 `{seed}` 必须等于该浏览器 session 的 active seed，并且 daemon lease 已
  attach 该 seed；网关不得把 URL seed 直接当作 header seed 透传。
- 所有带 `seed` 的 command / service / bootstrap / timeline 请求都必须满足上述
  seed↔lease 不变量；跨 seed 请求一律拒绝。
- 每个网关实例的默认硬上限：活跃浏览器 session 8、daemon lease 8；nonce 全局
  待兑换上限 256、每 IP 32、每 IP 签发 30/分钟、兑换 10/分钟。超限 fail-closed 并审计。

### 4.5 discovery 信任边界

- 网关只读取本机 `daemon.json`，不得接受浏览器传入 daemon endpoint/token。
- 只接受回环 endpoint：`127.0.0.1`、`localhost`、`[::1]`。
- 必须校验 pid 存活、endpoint 可达、`server_epoch`、协议版本与 build id。
- 禁止调用 `ensure_daemon_running` / `launch_daemon_if_missing` 或任何静默拉起路径。
- discovery 缺失、陈旧或轮换中一律 fail-closed，不自动降级到 LAN daemon。
- `server --bind 0.0.0.0` 的 LAN daemon 不得被本地 WebUI 网关接管。

### 4.6 seed 发现与切换

首期浏览器只允许 attach 已有 seed：

- `GET /__gateway/sessions` 的数据源明确为网关内部调用 daemon `session.list`，再脱敏
  后返回；该 daemon 方法不直接暴露给浏览器。
- 网关脱敏结果默认不返回 cwd、workspace 绝对路径、模型名和内部配置。
- `POST /__gateway/sessions/{seed}/attach` 是唯一 seed 切换入口，必须审计。
- 网关先用内部 `session.list` / `session.meta` 校验 seed 存在，再执行 §4.4 的
  重新协商；attach 后旧 seed 的 SSE 必须关闭，新 seed 未完成 attach 前不得建流。
- `session.new` / `session.resume` 是否开放留作产品决策；首期默认拒绝。
- 浏览器提交的 command envelope 中的 `seed` 必须被网关覆盖为当前 active seed；
  浏览器不能指定其它 seed。

### 4.7 命令与 service allowlist

命令首期 allowlist：

```text
SessionAttach             # 仅由 /__gateway/sessions/{seed}/attach 触发，必须审计
ConversationSendMessage
ConversationCancel
InteractionAskRespond
InteractionAskDismiss
PlanReviewRespond
ToolPermissionRespond
```

命令默认拒绝：

```text
SessionCreate / SessionResume / SessionClose
SessionArchive / SessionUnarchive / SessionDelete / SessionShutdown
AgentReloadConfig / SetToolMode
SkillsActivate / SkillsReload / SkillsOperation
ToolInvoke
ConversationUndoTurn / ConversationCompact
```

`SessionAttach` 仅建立 seed 归属、不触碰 actor；它与 `SessionResume` 不同，后者会触发
actor resume / 整包重建，首期继续拒绝。

service method 首期 allowlist：

```text
daemon.version
session.list              # 网关脱敏后返回
session.meta
session.activity
session.dashboard
session.get_activity
workspace.get
workspace.list
fs.list / fs.read         # 必须携带网关注入的 active-seed scope；daemon 组件级校验
todo.status / todo.list
plan.read / plan.context_stats
stats.token_usage
git.diff / git.branch / git.branches / git.file_diff
```

service method 默认拒绝：

```text
config.save / config.set_permission_level / profile.*
workspace.create / workspace.rename / workspace.delete
workspace.move_session / workspace.detach
todo.set
git.switch_branch / git.commit
skills.operation / skills.reload
subagent.spawn
session.set_tool_mode
```

`config.load` 不原样透传；若允许，必须定义独立脱敏读模型。

allowlist 分为两层：

- **网关内部调用 daemon**：允许使用完整 `session.list` / `session.meta` 做发现与校验；
- **浏览器可触发**：只能通过网关显式路由，网关必须覆盖 seed/scope 参数并禁止方法透传。

`fs.*` 的风险不是简单的“全局 vs 局部”，而是现有 `allowed_roots` 会把所有会话 cwd、
UI workspace 注册表和数据根取并集，导致跨会话互相越界。实现前必须增加显式作用域参数
（例如 `scope_seed`），在现有 `normalize_lexically` / `resolve_target_path` /
`path_within_dir` / `is_sensitive_session_path` 链之上，再叠加 active seed cwd 的
组件级前缀校验；既有防护不能删除，仅传 `seed` 但不增加该前缀约束也不构成收口。

### 4.8 审批 challenge 权威映射

浏览器只提交：

```json
{ "challenge_id": "...", "decision": "approve|reject|submit", "payload": {} }
```

网关将其映射到现有协议：

- ask / plan：`interaction_id`
- tool permission：`tool_call_id` 或 canonical interaction id

工具名、目标路径、风险等级、动作摘要和信任文件夹范围必须由 daemon / canonical state
生成，前端只展示。challenge 的 seed 绑定由 daemon 的 pending interaction / tool call
状态派生，浏览器不得提交 seed。challenge 必须带 TTL、一次性，并拒绝重放；网关只做
identity 映射，不得另造不可追溯的 challenge id。

### 4.9 `/health` 与 `/activity`

- `/health` 保持最小探活，不回显 token、token 长度或用户内容。
- `/activity` 默认要求认证；若必须免鉴权，只能存在于回环且返回最小字段。
- 网关只返回脱敏 activity 视图，不直接透传 daemon 原始响应。
- LAN `server` 模式必须重新定义这两个端点的策略，不能沿用当前免鉴权行为。

### 4.10 构建与进程边界

- 网关优先实现为独立 crate/binary；若必须合并到 `qaqh-daemon`，至少使用 compile-time
  feature，保证普通 `run` / `server` 构建不链接 WebUI embed 与静态资源。
- 固定 `QAQH_DEBUG_RENDERER_DIR` 只允许存在于显式开发模式。
- `qaqh-daemon webui` 仅是 UX 子命令，不得让普通 daemon 自动挂载网关；若 discovery
  指向非回环 `server`，该子命令必须直接拒绝启动。
- `is_authorized` 改常量时间比较，属于实现前置项；验收时检查实现选型（如 `subtle`）
  并覆盖 token 长度/前缀差异测试。

### 4.11 Cookie 续期、CSRF 与多标签页

- 浏览器 session 有独立 TTL，并与 daemon lease TTL 分开管理。
- 续期请求必须校验 Origin / CSRF；续期失败时清理 Cookie 与服务端映射。
- HTTP 回环模式不使用要求 `Secure` 的 `__Host-` cookie 前缀。
- 首期建议一个浏览器 profile 共享一个 session 和一个 active seed；多标签页共享状态，
  因此任一标签页切换 active seed 会影响其它标签页，UI 必须在切换前明确提示。
  若产品要求标签页隔离，应改为每标签页独立 session id 与独立 daemon lease，不能只靠
  前端内存区分。
- logout、网关重启、daemon epoch 变化都必须使服务端 session 失效。

---

## 5. 浏览器安全边界

### 5.1 Origin、Host 与 CSRF

- 网关只接受回环来源：`127.0.0.1`、`localhost`、`[::1]`。
- Host 白名单必须包含实际端口校验。
- Cookie 认证的写请求必须校验精确 `Origin`。
- `Sec-Fetch-Site` 只能作为纵深防御，不能单独作为认证。
- 无 `Origin` 的请求只允许 Bearer 路径，不允许 Cookie 路径。
- 状态变更接口增加 CSRF token 或等价的同源校验。
- 禁止通配 CORS；默认不返回 `Access-Control-Allow-Origin`。

### 5.2 响应头

网关必须至少下发：

```text
Content-Security-Policy:
  default-src 'none';
  script-src 'self';
  style-src 'self' 'unsafe-inline';
  img-src 'self' data:;
  font-src 'self';
  connect-src 'self';
  frame-ancestors 'none';
  base-uri 'none';
  form-action 'none';
  object-src 'none';

Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Resource-Policy: same-origin
X-Content-Type-Options: nosniff
X-Frame-Options: DENY
Referrer-Policy: no-referrer
```

`style-src 'unsafe-inline'` 仅在前端确实依赖内联样式时临时保留；移除该豁免必须列入
§7.3 验收项，不能作为“后续优化”悬挂。

### 5.3 前端注入

所有模型输出、工具输出、timeline、原始事件都按不可信输入处理。

必须：

- markdown 链接只允许 `http` / `https`。
- 禁止 `javascript:`、`data:`、`file:`、`blob:`、`vbscript:` 等 scheme。
- 外链增加 `rel="noopener noreferrer"`。
- 图片默认只允许同源或明确 allowlist；不得由模型输出触发任意外部请求。
- 不使用 `innerHTML` / `insertAdjacentHTML` / `document.write` 渲染模型数据。
- 原始事件抽屉继续只用文本节点。
- 对 HTML entity、大小写、Unicode、空白字符绕过做回归测试。

### 5.4 点击劫持与审批

WebUI 包含“批准 / 拒绝 / 信任文件夹”等按钮，属于安全边界：

- `frame-ancestors 'none'` + `X-Frame-Options: DENY` 必须有。
- 审批 ID 必须由服务端签发，绑定 tool call 与 seed，带 TTL，不可重放。
- 风险级别、目标路径、动作摘要必须由服务端权威生成，前端只展示。
- 模型输出不能控制审批按钮的语义、目标或风险标签。
- 高风险操作不得由脚本自动点击完成。

### 5.5 nonce 与限流

- nonce 默认硬上限：全局 pending 256、每 IP pending 32、每 IP 签发 30/分钟、
  每 IP 兑换 10/分钟；超限 fail-closed。
- 活跃浏览器 session 与 daemon lease 默认上限均为 8，超过后拒绝创建。
- nonce 兑换必须一次性、短 TTL、响应 `no-store`。
- 网关只返回不透明会话，不返回 daemon token。
- 静态资源、bootstrap、session、命令、SSE 使用不同的 body limit 与速率策略。
- 错误响应不得回显 token、nonce 内容或内部路径。

### 5.6 静态资源与构建

- 生产只服务编译时嵌入的构建产物。
- release 不允许 `QAQH_DEBUG_RENDERER_DIR` 指向任意目录。
- 禁止目录列表、隐藏文件、source map 和任意扩展名。
- 路径解析使用组件级白名单，并处理软链接逃逸。
- 前端依赖锁定版本并纳入供应链审计。
- `webui/out/renderer` 为构建产物，不入库；构建步骤必须可重复。
- 构建产物应带 build id/hash，便于确认浏览器与 daemon 同批次。

---

## 6. 迁移方案

### Phase 0：冻结边界

- 评审本文并确认 `qaqh-daemon webui` 的进程形态。
- 确认 `/debug` 退役策略。
- 确认浏览器 principal 的能力矩阵。
- 确认 WebUI 源码在后端仓库中的目录布局。

### Phase 1：引入网关骨架

- 在 `qaqh-daemon` 增加 `webui` 子命令（已完成）。
- 新增独立网关 crate/module，默认只监听回环（已完成，`crates/qaqh-webui-gateway`）。
- 普通 `run` / `server` 不挂载任何 WebUI 路由（已完成）。
- 增加 discovery 读取、daemon 可达性检查、优雅退出（已完成）。

### Phase 2：接入 WebUI 源码与构建

- 把临时 WebUI 源码迁入后端仓库的稳定目录。
- 移除 Bun sidecar 作为生产路径；保留为可选开发工具。
- 统一 `bun run build` 与 Rust embed/build.rs 路径。
- 生产网关使用构建产物，开发模式可使用 `QAQH_WEBUI_DEV_DIR`。

### Phase 3：浏览器会话

- 实现 nonce -> 不透明浏览器会话。
- 实现 HttpOnly / SameSite=Strict Cookie。
- 移除仓外临时前端使用的 `?__lease=` accommodation 与浏览器可见 Bearer。
- 增加 Origin、CSRF、Host、端口校验。
- 增加方法 allowlist 与 seed scope。

### Phase 4：前端安全收口

- markdown URL scheme 白名单。
- CSP 与外链/图片策略。
- 原始事件、tool output、diff 的安全渲染回归测试。
- 审批卡片服务端权威化。

### Phase 5：`/debug` 退役

- 普通 daemon 删除 `/debug` 路由。
- 删除或隔离 nonce/token 兑换桥。
- 若保留诊断模式，改为独立开关与独立命名空间。
- 更新 README、CLI help、docs 索引和安装说明。

---

## 7. 验收标准

### 7.1 默认关闭

- `qaqh-daemon run` 启动后，`GET /debug/`、`GET /ui/`、`GET /__gateway/bootstrap.js`
  均不存在对应路由。
- `qaqh-daemon server` 默认同样不提供 WebUI。
- 代码层验证：普通模式构建的 Router 中不存在 WebUI route。

### 7.2 显式网关

- `qaqh-daemon webui` 启动独立网关并打印实际地址。
- 默认只监听 `127.0.0.1`。
- 网关退出后，daemon 继续服务 Ringing API。
- 网关重启后旧浏览器会话失效。

### 7.3 浏览器安全

- `document.cookie` 看不到 HttpOnly 会话值。
- 浏览器网络面板中不存在 daemon Bearer。
- SSE URL 中不存在 token / lease 查询参数。
- 跨站 POST、缺失/伪造 Origin、点击劫持、nonce 重放全部失败。
- CSP 阻断外部脚本、外部连接和未授权图片。
- `javascript:` / `data:` markdown 链接无法触发脚本。
- `style-src 'unsafe-inline'` 的移除有明确回归验证或证明前端不依赖内联样式。

### 7.4 授权范围

- 浏览器会话不能读取未 attach 的 seed。
- URL seed 与 active seed 不一致时，bootstrap / timeline / SSE / service 全部拒绝。
- 浏览器会话不能调用 `/control/v1/stop`。
- 浏览器会话不能透传调用未列出的 service method。
- `fs.read` / `fs.list` 不能越过 active seed cwd；daemon 的全局 `allowed_roots` 不能单独
  作为放行依据。
- 审批 ID 不可伪造、不可重放，过期后拒绝。

### 7.5 回归与审计

- `cargo check --workspace`
- `cargo clippy --workspace --all-targets`
- `cargo test --workspace`
- WebUI `bun run typecheck` / `bun run build`
- 网关 e2e：启动、会话、SSE、审批、退出
- 安全 e2e：跨站、rebinding、XSS、nonce 洪水、路径穿越、跨 seed
- 常量时间鉴权：token 长度/前缀差异不产生单调比较耗时
- `GET /health` 响应体不含 token、token 长度或用户内容
- 限流 e2e：8 个 session/lease、256 个 pending nonce、32/IP、30 签发/分钟、10 兑换/分钟
- 日志记录：启动、关闭、session exchange、origin 拒绝、scope 拒绝、审批结果
- 日志不得包含 Bearer、nonce 明文或完整敏感路径

---

## 8. 已冻结决策

> 冻结日期：2026-09-20。以下均采用“推荐”列；后续实现若需改变，必须另开设计变更并重新评审。

| ID | 决策 | 推荐 |
|---|---|---|
| D-1 | 独立网关进程 vs daemon 内临时路由 | **独立进程** |
| D-2 | `webui` 子命令是否自动启动 daemon | **默认要求 daemon 已运行；首期不提供代启参数** |
| D-3 | 默认端口 | **随机端口 + 打印 URL；固定端口必须显式 `--port`** |
| D-4 | 浏览器是否允许 `config.save` | **默认拒绝；只允许独立脱敏读模型** |
| D-5 | 是否保留诊断 `/debug` | **普通 daemon 删除；首期不提供诊断兼容路由** |
| D-6 | WebUI 源码目录 | **仓库根 `webui/`，构建产物 `webui/out/renderer`** |
| D-7 | 浏览器会话是否允许 `fs.*` | **只允许 active seed cwd，daemon 显式作用域校验** |
| D-8 | 前端是否继续使用 Bun sidecar | **生产不用；仅开发/调试可选** |
| D-9 | 多标签页是否共享浏览器 session / daemon lease | **首期共享；切 seed 会打断其它标签页，UI 必须提示** |
| D-10 | 是否开放 `session.new` / `session.resume` | **首期只 attach 已有 seed** |

---

## 9. 与现有架构的关系

- 本文不改变 Ringing V1 wire；浏览器网关只是 Ringing 的受限客户端。
- 本文不改变 session canonical fact / projection 方向；新合入的
  `SessionMetaProjection`、`ResourceProjection`、`ControlProjection`、
  `ConversationProjection`、`TimelineProjection`、`ProjectionSet`、reliable replay 与
  `ContentClockRecord` 仍是 WebUI 的只读投影来源。
- 本文要求浏览器消费 typed projection，不直接读存储布局。
- 本文与 v2 总架构的 `Node transport` 边界一致：WebUI 是 transport 之上的客户端，
  不是 daemon 存储或 actor 的第二所有者。

---

## 10. 一句话验收

> 普通 daemon 启动后，机器上不存在浏览器控制面；只有显式运行 `qaqh-daemon webui`
> 才会出现一个仅回环、临时、受限、可审计的 WebUI 网关，浏览器从头到尾看不到 daemon
> Bearer token。
