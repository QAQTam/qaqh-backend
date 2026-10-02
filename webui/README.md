# QAQ-Harness WebUI(Tauri 桌面壳)

按 `agent-webui-phase1-spec.md`(v1.0)实现的对话页:SolidJS 2.0 rc + Vite +
TypeScript strict,运行在 **Tauri 2 桌面壳**(`webui/src-tauri`,crate
`qaqh-webui-app`)内。webui 浏览器 gateway 部署形态已随 Tauri 化移除
(plan-webui-tauri C3/D1)。

## 构建

```bash
bun install
bun run typecheck
bun run test      # 仅纯逻辑单测(spec §1.3)
bun run build     # 输出 out/renderer/,由 tauri.conf.json 的 frontendDist 消费
```

## 开发(桌面壳)

```bash
just desktop-dev   # 构建 daemon(debug)+ 放置 sidecar + bun tauri dev
```

`tauri dev` 会先跑 `bun run dev`(vite :5173),Rust 宿主以 devUrl 打开 webview;
宿主经 `qaqh-client` 直连 daemon——discovery 有兼容实例则复用,否则拉起
`target/debug/qaqh-daemon`(可用 `QAQH_DAEMON_PATH` 覆盖)。

## 数据单源(架构契约)

| 事实 | 唯一来源 | 前端角色 |
|---|---|---|
| transcript 结构/文本 | timeline(宿主转发的快照 + `timeline://entry`,`watermark` 去重,缺口→快照校正) | 纯 reducer 投影 |
| 审批/AskUser 待处理 | `pending_approvals` IPC(challenge 由宿主签发,canonical id 不出宿主) | 投影事件只当刷新信号 |
| 会话标题/运行态/后台「有新回复」点 | `session_list` IPC 轮询(turn_count/running) | 对比渲染 |
| diff | 后端 unified diff 文本(`display.body.diff.unified`) | 解析渲染,不重算 |
| 工具展示 | 后端 `display` 投影(header/body/metrics) | 直接消费;`args_json` 仅作未注册回退 |

## 宿主契约(为什么只有活动标签持流)

Rust 宿主维护「单一 active seed 持流」(plan-webui-tauri D4):切标签 =
attach(宿主切 active + 重建 timeline/v2 流)+ 快照重建。重连/退避/续传责任在
宿主(qaqh-client 的 V2Stream/timeline 流自带重连),前端 `timeline://status`
驱动 connection 信号;offline 后由用户动作(重试按钮/窗口聚焦)触发
`streams_retry`。daemon lease 本身支持一个 client 拥有多个 seed,放开并行流
属上线后增强。

窗口关闭 = 宿主 detach(`Client::close()`),daemon 与运行中的 Turn/待审批
继续存活(§5.3 语义);宿主退出**不**调用 `stop_daemon`。

## 已知契约缺口(实现内的 `[契约缺口]` 注释)

- **TS-1** 后端 `TimelineEntry` 无 epoch-ms 时间戳 → 耗时/偏移用客户端时钟采样,
  集中在 `src/lib/time.ts`;后端补 `ts` 后换一处即可转正。
- **A-1** 授权 challenge details 不携带 `choices[]` → 按宿主 `command_for` 的
  固定决策集渲染;后端补 choices 后按列表渲染。
- **D-1** 后端 diff 无结构化 `stats` → 由后端 diff 文本行数统计;后端补
  stats 后前端统计退役。

## 安全边界

- **token 面**:daemon bearer token / lease id 只存在于 Rust 宿主进程;
  webview 可达面仅类型化 IPC + 宿主转发事件(devtools 可核验)。
- **审批防御纵深**:webview 只见宿主签发的不透明 challenge id(64 hex,
  TTL 5min,一次性消费 + active-seed scope 校验),canonical `call_*`/`int_*`
  id 不出宿主。
- Markdown:marked + DOMPurify,远程图片降级为文字链接,外链经宿主
  `open_external` 走系统浏览器(仅 http/https)。
- diff/工具输出:textContent 构建,不经 innerHTML;ANSI 经 anser 解析为节点。
- 工具行内无手绘 SVG(unplugin-icons + lucide);无打包字体;CSP 由
  `tauri.conf.json > app.security.csp` 单点接管(`default-src 'self'` + ipc)。
- 外链/导航:webview 内不导航外站;IPC 面即 `invoke_handler` 注册白名单,
  `service_rpc` 另有方法白名单(与原 gateway 代理面一致)。
