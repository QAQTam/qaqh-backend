# WebUI Tauri 化与 gateway 移除计划(webui-tauri)

> 状态:规划稿,2026-10-02 立项。
> 来源:webui 按 `agent-webui-phase1-spec.md` v1.0 重写完成(2026-10-02,前版 Codex 风
> 草稿整体移除)之后的架构分析;单源审计已翻正(工具展示读后端 `display` 投影、
> approval 单源 RPC、timeline watermark 去重 + 缺口→快照)。
> 已定决策:sidecar 直接上;上线工程(签名/更新器/分发)推迟到上线阶段再议。

## 0. 现状盘点(事实)

- 三层链路:浏览器 `webui/`(Solid,transport 已抽象)→ `qaqh-webui-gateway`
  (2855 行,浏览器粘合层)→ daemon ringing v2 HTTP/SSE(协议本体)。
- gateway 按职责拆解与去向:
  - daemon 发现/校验/HTTP 客户端 → 删(`qaqh-client` 同功能覆盖);
  - nonce 引导 + HttpOnly 会话 + CSRF + origin 白名单 + 限流 → 删(webview 是
    可信壳,daemon token 只留在 Rust 宿主);
  - 审批 challenge 不透明化(approval.rs,~250 行)→ **移植**进宿主(防御纵深);
  - `sanitize_session_list` → 移入 `qaqh-client`(与 TUI 共享);
  - 命令白名单 → 删(IPC 面为类型化命令,`invoke_handler` 注册即白名单);
  - 静态资源内嵌 + `build.rs` → 删(Tauri `frontendDist` 接管)。
- daemon 侧 webui 专属代码仅三处:main.rs `webui` 子命令(纯启动器,~40 行)、
  justfile 三个 recipe、`GET /ringing/v2/sessions/{seed}/approvals` 路由
  (保留,见 D3)。
- `qaqh-client`(5026 行)为 TUI/桌面壳准备的共享传输:**仓内目前零消费者**
  (仅 examples)。已具备:discovery 校验 + 按需拉起(`launch_daemon_if_missing`/
  `daemon_path`)、lease 自动续期(`RingingSession::run_renewal`)、v2 单流订阅
  (`on_v2_event/reset/status`)、per-session timeline 流(`activate/deactivate_
  timeline`,支持多会话并行)、类型化命令、`service_v2` RPC、`bootstrap_v2`、
  `fetch_timeline_page`(before_index 翻页)、content 上/下载、`RemoteEndpoint`
  (远端 daemon)、`stop_daemon`。**缺**:`pending_approvals()`(D3)。
- discovery 为全局单例:`data_dir()/daemon.json` + `daemon.lock`;多前端共享同一
  daemon 实例是既定语义。
- 契约缺口(新 webui 内有 `[契约缺口]` 注释):TS-1(timeline 条目无 epoch-ms
  时间戳,耗时用客户端时钟;注意 projection 信封 `ts_ms` 已在 W1 落地,但
  timeline `TimelineEntry` 仍无 ts)→ 依赖后端补字段,不在本计划内;A-1(授权
  details 无 `choices[]`)、D-1(diff 无结构化 `stats`)同属后端,不阻塞本计划。

## 1. 目标与非目标

**目标**
1. 桌面壳:Tauri 2(WebView2 / WKWebView),前端复用现有 `webui/` 构建产物。
2. daemon 以 sidecar 随包分发,版本与 app 锁定。
3. 删除 `qaqh-webui-gateway` 与 daemon `webui` 子命令;daemon ringing v2 API
   与其他客户端(TUI/CLI)零改动。

**非目标(上线阶段另行立项)**
- 签名、更新器(tauri updater)、分发渠道与自动更新策略;
- 放开"后台标签并行 timeline 流"(spec §5.2 完整版,现状:活动标签独占连接 +
  sessions 轮询驱动状态点,该限制随 gateway 移除自然解除,放开属增强);
- TS-1/A-1/D-1 的后端侧补齐;
- spec 第二阶段功能(设置页、会话列表管理、多窗口)。

## 2. 决策记录

| # | 决策 | 说明 |
|---|---|---|
| D1 | sidecar 直接上,跳过"复用已安装 daemon"过渡 | 启动策略:读 discovery → 兼容(同 lane、protocol_version 满足)则复用在跑 daemon;不兼容 → 报错并给动作(退出旧实例/一键停止后拉起 sidecar),**不静默杀**(会砍掉 TUI 在跑的 Turn);app 自身多开用 single-instance 插件挡 |
| D2 | 浏览器模式随 gateway 一起删除 | `GatewayTransport` 仅在 A/B 阶段作为对照保留,C 阶段删除 |
| D3 | daemon `/approvals` 投影路由保留 | 注释由"本地浏览器网关"改为"本地壳层审批投影";`qaqh-client` 新增 `pending_approvals()` 包装 |
| D4 | 第一阶段维持"单活动标签持流" | 后台标签状态点继续走 sessions 轮询;多 timeline 流放开为上线后增强 |
| D5 | 上线工程推迟 | C 阶段只要求本地 `tauri build` 产出可运行的自包含安装包并人工验收;签名/更新器不做 |

## 3. 目标架构

```
┌ Tauri 2 app(webui/src-tauri,crate 名 qaqh-webui-app)
│  webview: webui/ 构建产物(frontendDist ../out/renderer,devUrl :5173)
│  Rust 宿主:
│    daemon.rs    discovery 兼容校验 + sidecar 拉起 + 生命周期(D1 策略)
│    commands.rs  IPC 请求/响应面(下表)
│    events.rs    ClientHandlers 回调 → app.emit(信封与 SSE 帧同形)
│    challenge.rs approval.rs 移植(不透明 id ↔ canonical id,TTL/一次性消费)
└ qaqh-daemon(sidecar;与既有安装共享 data_dir,sessions/discovery 语义不变)
```

**IPC 请求/响应面(commands.rs,与前端 transport 方法一一对应)**

| command | 参数 → 返回 | qaqh-client 落点 |
|---|---|---|
| `session_list` | () → Vec\<SessionMeta\>(sanitize 后字段同 gateway) | `service_v2("session.list")` + sanitize 迁入 |
| `attach` | (seed) → () | `attach()`(含 lease 建立) |
| `pending_approvals` | () → Vec\<ApprovalView\> | 新增 `pending_approvals()`(走 daemon `/approvals` 路由) |
| `respond_approval` | (challenge_id, decision, payload) → () | `send_command_v2_typed`(challenge 经 challenge.rs 映射) |
| `send_message` | (seed, text) → ack | `send_command_v2_typed(conversation_send_message)` |
| `cancel_turn` | (seed) → ack | `conversation_cancel` |
| `create_session` | () → seed(轮询 sessions diff 由前端负责,与现状一致) | `SessionCreate` |
| `timeline_page` | (seed, limit, before_index) → page(含 server_epoch/has_more/truncated_before) | `fetch_timeline_page` |
| `service_rpc` | (method, params) → Value,**方法白名单** | `service_v2` |
| `open_external` | (url) → (),仅 http/https | tauri opener 插件 |
| `streams_retry` | (seed) → () | 触发宿主侧重连(用户点「重试」/窗口聚焦) |

**事件面(events.rs,`ClientHandlers` → Tauri event,payload 与现行 SSE 帧同形)**
- `timeline://entry` ← `on_timeline_entry`: `{ session_id, entry }`
- `timeline://status` ← `on_timeline_status`(前端 connection 信号由此驱动,**重连/退避/续传责任上移宿主**,前端 `lib/reconnect.ts` 的自动退避逻辑在 Tauri 后端下不再使用)
- `projection://event` ← `on_v2_event`: 信封原样(cursor/session_id/stream_key/payload)
- `projection://reset` / `conn://liveness` ← 对应 handler

前端不变的部分:reducer 的 watermark 去重、seq 缺口→快照校正(对无序/丢帧的
Tauri 事件通道是天然兜底)、审批单源 RPC、分页/淘汰、思考链、diff 渲染。

## 4. 阶段与任务

### A. 前端预备(无后端改动,可立即开工)

- [ ] A1 `lib/transport.ts` 拆为 `TransportBackend` 接口 + `GatewayTransport`
  (现实现平移)+ `TauriTransport` 空壳;运行时按 `window.__TAURI_INTERNALS__`
  选择。store/组件零改动(接口方法名即现方法名)。
- [ ] A2 CSP 注入插件改为按目标条件生效(`__TAURI__` 构建下关闭,由
  `tauri.conf.json > app.security.csp` 接管,避免双重 CSP)。
- [ ] A3 验收:`bun run typecheck / test / build` 全绿,GatewayTransport 行为回归
  (浏览器 preview 冒烟)。

### B. 宿主 MVP(sidecar 接线,`tauri dev` 对真实 daemon 走通)

- [ ] B1 新建 `webui/src-tauri`(crate `qaqh-webui-app`),加入 workspace members
  (共享 `[workspace.lints]`/profile;`cargo build --workspace` 会带上它,可接受)。
  `tauri.conf.json`:identifier、窗口 1200×800(最小尺寸约束)、`frontendDist
  ../out/renderer`、`beforeDevCommand bun run dev` / `beforeBuildCommand bun run
  build`、`externalBin` 指向 sidecar、CSP(默认源 'self' + ipc)。
- [ ] B2 宿主 daemon 生命周期(D1 策略):读 discovery → 兼容校验(lane/
  protocol_version)→ 复用;不兼容 → 错误事件 `conn://incompatible`(前端渲染
  明确文案 + 动作);无 daemon → 拉起 sidecar(dev 下 `daemon_path` 指
  `target/debug/qaqh-daemon`,`ClientOptions` 默认值即此);single-instance 插件。
- [ ] B3 commands.rs 全量命令(上表)+ `invoke_handler` 白名单注册。
- [ ] B4 events.rs 事件转发 + 背压观察(高吞吐回合下 webview 无积压告警;
  必要时宿主合并 text_delta 帧再 emit——注意不得破坏 `timeline_seq` 连续性,
  缺口→快照路径依赖它)。
- [ ] B5 `challenge.rs`:gateway `approval.rs` 移植(TTL、一次性消费、scope 校验)。
- [ ] B6 `qaqh-client`:`pending_approvals()` + `sanitize_session_list` 迁入;
  带 cargo 单测。
- [ ] B7 前端 `TauriTransport` 完整实现(listen 订阅 + invoke;connection 状态由
  `timeline://status` 驱动;offline→`streams_retry`)。
- [ ] B8 前端 Tauri 专属改造:外链 hook 改 `open_external`;标题栏
  `data-tauri-drag-region` 并入 `#top` + `--titlebar-inset-right` 占位(spec §2.2)。
- [ ] B9 justfile `desktop-dev` recipe(web-build 前置 + `bun tauri dev`)。
- [ ] B10 验收:dev 模式完整会话流——新建/发消息/流式渲染/工具卡/diff/审批卡
  (含 high risk)/断网重连/翻页/淘汰/切标签;`bun run typecheck / test` +
  workspace cargo test 全绿。

### C. 切换与自包含安装包

- [ ] C1 release 构建链:`just desktop-build` = web-build + `cargo build
  --release -p qaqh-daemon` + sidecar 按目标三元组改名放置 + `bun tauri build`。
- [ ] C2 本地安装包人工验收(Windows 优先):全新环境(无已装 daemon)安装 →
  拉起 sidecar → 完整会话;有旧 daemon 在跑 → D1 不兼容路径文案正确。
- [ ] C3 默认后端切 Tauri,删除 GatewayTransport 与浏览器 preview 相关脚本。
- [ ] C4 文档:webui/README(双后端章节改单后端)、根 README、spec §2.2 勾掉
  "预留"注记;`docs/audit-security-*.md` 跟进一节(新信任边界:token 仅宿主)。

### D. 移除 gateway

- [ ] D1 删 `crates/qaqh-webui-gateway/` 整目录(含 build.rs 资产占位逻辑)。
- [ ] D2 删 daemon main.rs `webui` 子命令与 `run_webui_gateway`。
- [ ] D3 删 justfile `web` / `web-build` 旧 recipe(build-webui-gateway 由
  desktop-build 取代);workspace members 移除 gateway 行。
- [ ] D4 daemon `/approvals` 路由注释改为"本地壳层审批投影"(D3 决策)。
- [ ] D5 全量回归:`cargo test --workspace` + webui 全套 + B10 清单在安装包上
  重跑;确认 TUI/CLI 路径零受影响(ringing v2 API 未动)。

## 5. 验收清单(对应 spec §19 与桌面新增项)

1. spec §19 全项在 Tauri 壳下复跑,重点:13(重连/快照校正)、14(长上下文)、
   11(动画/reduced-motion)、4(思考链)。
2. 桌面新增:窗口关闭 ≠ 取消 Turn/审批(§5.3 语义,宿主 detach 不断后端);
   外链走系统浏览器;无 daemon token 出现在任何 webview 可达面(devtools 检查)。
3. sidecar:全新环境可从零拉起;`data_dir` 与既有安装共享且互不破坏。

## 6. 风险与对策

| 风险 | 对策 |
|---|---|
| Tauri 事件无 SSE 的顺序/不丢保证 | watermark 去重 + 缺口→快照已兜底;B4 观察高吞吐背压,必要时宿主合并 delta(保 `timeline_seq` 连续) |
| 双 CSP 相互削弱 | A2 条件注入,生产仅 tauri.conf.json 一处 |
| WKWebView 与 WebView2 行为差异 | B10/C2 双平台各跑一轮(当前先 Windows) |
| lease 生命周期绑 app | 宿主退出走 `close()`(detach),不触 `stop_daemon`;窗口全关 = detach,与 §5.3 一致 |
| 旧 daemon 占位(discovery 单例) | D1 兼容校验 + 明确报错文案,不静默杀 |
| externalBin 目标三元组命名/放置错误 | C1 的改名步骤写成 justfile 函数并加存在性断言 |
| `qaqh-client` 首次被仓内正式消费暴露隐性问题 | B6/B10 的单测与完整会话验收即首轮覆盖;发现缺陷就地修在 client(与 TUI 共享受益) |

## 7. 遗留(上线阶段再议)

- 签名 / tauri updater / 分发渠道(D5)。
- 放开并行多 timeline 流,状态点从轮询变事件驱动(spec §5.2 完整版)。
- TS-1 转正:后端为 `TimelineEntry` 补 epoch-ms `ts`,前端 `lib/time.ts` 换一处
  采样源即可;顺带 A-1(`choices[]`)、D-1(diff `stats`)一并谈。
- `/approvals` 是否折叠进 `bootstrap_v2` ControlState(减少一条专用路由)。
