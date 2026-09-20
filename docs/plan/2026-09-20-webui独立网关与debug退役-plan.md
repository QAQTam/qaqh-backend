# WebUI 独立网关与 `/debug` 退役设计（草案）

> 日期：2026-09-20
> 基线：`betav2 @ 8e6b777`（PR #144 merge）
> 状态：**草案待评审**。本文只冻结方向与安全边界，不代表已经实现。
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

### 2.1 当前实现事实

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
- 临时 WebUI 使用 Bun bridge 注入 Bearer：
  `qaqh-webui-temp/webui/bridge.ts:77`。
- 前端 SSE 通过 `?__lease=` 传递 lease，属于临时 accommodation：
  `qaqh-webui-temp/webui/src/lib/ringing.ts:190-220`。
- markdown 渲染库直接设置 `href` / `src`，没有 URL scheme 白名单：
  `qaqh-webui-temp/webui/node_modules/streaming-markdown/smd.js:1621-1622`。
- `/health`、`/activity` 不在 `/debug` 前缀下，不受 `/debug` 回环守卫约束：
  `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:135-136`。

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
GET  /                        静态 SPA
GET  /assets/*                静态资源
GET  /__gateway/bootstrap.js  一次性 nonce
POST /__gateway/session       nonce -> HttpOnly browser session
GET  /ringing/*               代理到 daemon，注入 Bearer
POST /ringing/*               代理到 daemon，注入 Bearer
```

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
6. 网关 Set-Cookie: HttpOnly; SameSite=Strict; Path=/ringing
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
| `fs.list` / `fs.read` | 限定当前 seed 的 workspace 根 |
| `config.load` / `config.save` | 默认拒绝或仅返回脱敏读模型 |
| `workspace.delete` / 跨 workspace 操作 | 默认拒绝 |
| `/control/v1/stop` / `stop-if-idle` | 拒绝 |
| 任意方法名透传 | 拒绝；网关使用明确 allowlist |

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

`style-src 'unsafe-inline'` 仅在前端确实依赖内联样式时保留；后续应移除。

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

- nonce 存储增加全局上限、每 IP/连接上限和签发速率限制。
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

- 在 `qaqh-daemon` 增加 `webui` 子命令。
- 新增独立网关 crate/module，默认只监听回环。
- 普通 `run` / `server` 不挂载任何 WebUI 路由。
- 增加 discovery 读取、daemon 可达性检查、优雅退出。

### Phase 2：接入 WebUI 源码与构建

- 把临时 WebUI 源码迁入后端仓库的稳定目录。
- 移除 Bun sidecar 作为生产路径；保留为可选开发工具。
- 统一 `bun run build` 与 Rust embed/build.rs 路径。
- 生产网关使用构建产物，开发模式可使用 `QAQH_WEBUI_DEV_DIR`。

### Phase 3：浏览器会话

- 实现 nonce -> 不透明浏览器会话。
- 实现 HttpOnly / SameSite=Strict Cookie。
- 移除 `?__lease=` 与浏览器可见 Bearer。
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

### 7.4 授权范围

- 浏览器会话不能读取未 attach 的 seed。
- 浏览器会话不能调用 `/control/v1/stop`。
- 浏览器会话不能透传调用未列出的 service method。
- `fs.read` / `fs.list` 不能越过当前 seed 的 workspace 根。
- 审批 ID 不可伪造、不可重放，过期后拒绝。

### 7.5 回归与审计

- `cargo check --workspace`
- `cargo clippy --workspace --all-targets`
- `cargo test --workspace`
- WebUI `bun run typecheck` / `bun run build`
- 网关 e2e：启动、会话、SSE、审批、退出
- 安全 e2e：跨站、rebinding、XSS、nonce 洪水、路径穿越、跨 seed
- 日志记录：启动、关闭、session exchange、origin 拒绝、scope 拒绝、审批结果
- 日志不得包含 Bearer、nonce 明文或完整敏感路径

---

## 8. 待评审决策

| ID | 决策 | 推荐 |
|---|---|---|
| D-1 | 独立网关进程 vs daemon 内临时路由 | 独立进程 |
| D-2 | `webui` 子命令是否自动启动 daemon | 默认要求 daemon 已运行；显式参数才代启 |
| D-3 | 默认端口 | 随机端口 + 打印 URL；固定端口必须显式 |
| D-4 | 浏览器是否允许 `config.save` | 默认只读或脱敏；写配置需单独能力 |
| D-5 | 是否保留诊断 `/debug` | 默认删除；必要时独立 `--diagnostics` |
| D-6 | WebUI 源码目录 | 建议仓库根 `webui/`，构建产物 `webui/out/renderer` |
| D-7 | 浏览器会话是否允许 `fs.*` | 只允许当前 seed workspace 根 |
| D-8 | 前端是否继续使用 Bun sidecar | 生产不用；仅开发/调试可选 |

---

## 9. 与现有架构的关系

- 本文不改变 Ringing V1 wire；浏览器网关只是 Ringing 的受限客户端。
- 本文不改变 session canonical fact / projection 方向；新合入的
  `SessionMetaProjection`、`ResourceProjection` 仍是 WebUI 的只读投影来源。
- 本文要求浏览器消费 typed projection，不直接读存储布局。
- 本文与 v2 总架构的 `Node transport` 边界一致：WebUI 是 transport 之上的客户端，
  不是 daemon 存储或 actor 的第二所有者。

---

## 10. 一句话验收

> 普通 daemon 启动后，机器上不存在浏览器控制面；只有显式运行 `qaqh-daemon webui`
> 才会出现一个仅回环、临时、受限、可审计的 WebUI 网关，浏览器从头到尾看不到 daemon
> Bearer token。
