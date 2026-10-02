# QAQ-Harness WebUI(Phase 1)

按 `agent-webui-phase1-spec.md`(v1.0)实现的新对话页:SolidJS 2.0 rc + Vite +
TypeScript strict。旧版(Codex 风草稿:侧边栏会话列表 + 设置页)已整体移除,
本次为按 spec 的重写。

## 构建

```bash
bun install
bun run typecheck
bun run test      # 仅纯逻辑单测(spec §1.3)
bun run build     # 输出 out/renderer/,由 qaqh-webui-gateway 编译期内嵌
```

justfile 的 `web-build` 流程与本目录脚本名保持一致。

## 开发

```bash
cargo run -p qaqh-daemon -- webui --port 8642   # 显式端口起网关
WEBUI_GATEWAY=http://127.0.0.1:8642 bun run dev  # vite 代理 /__gateway
```

开发代理只转发 `/__gateway`;浏览器仍然只持有网关 HttpOnly 会话 cookie 与
内存 CSRF token,daemon token 不进浏览器。

## 数据单源(架构契约)

| 事实 | 唯一来源 | 前端角色 |
|---|---|---|
| transcript 结构/文本 | timeline 快照 + SSE(`last_event_id` 续传,`watermark` 去重,缺口→快照校正) | 纯 reducer 投影 |
| 审批/AskUser 待处理 | 网关 `POST /__gateway/approvals`(challenge 由网关签发) | 投影事件只当刷新信号 |
| 会话标题/运行态/后台「有新回复」点 | `GET /__gateway/sessions` 轮询(turn_count/running) | 对比渲染 |
| diff | 后端 unified diff 文本(`display.body.diff.unified`) | 解析渲染,不重算 |
| 工具展示 | 后端 `display` 投影(header/body/metrics) | 直接消费;`args_json` 仅作未注册回退 |

## 网关契约约束(为什么只有活动标签持流)

网关的 per-seed 面(命令/审批/RPC/timeline SSE/快照)全部限定在它的单一
active session 上(`qaqh-webui-gateway/src/lib.rs` 的 `seed_scope_violation`
检查),因此 UI 采用「活动标签独占连接」模型:切标签 = attach + 快照重建。
daemon 侧 lease 本身支持一个 client 拥有多个 seed,若后续放宽网关检查,
后台标签可升级为实时流(状态点从轮询变为事件驱动)。

## 已知契约缺口(实现内的 `[契约缺口]` 注释)

- **TS-1** 后端 `TimelineEntry` 无 epoch-ms 时间戳 → 耗时/偏移用客户端时钟采样,
  集中在 `src/lib/time.ts`;后端补 `ts` 后换一处即可转正。
- **A-1** 授权 challenge details 不携带 `choices[]` → 按网关 `command_for` 的
  固定决策集渲染;后端补 choices 后按列表渲染。
- **D-1** 后端 diff 无结构化 `stats` → 由后端 diff 文本行数统计;后端补
  stats 后前端统计退役。

## 安全边界

- Markdown:marked + DOMPurify,远程图片降级为文字链接,外链 `noopener noreferrer`。
- diff/工具输出:textContent 构建,不经 innerHTML;ANSI 经 anser 解析为节点。
- 工具行内无手绘 SVG(unplugin-icons + lucide);无打包字体;严格 CSP
  (仅生产构建注入 `meta[Content-Security-Policy]`,禁止内联/远程脚本)。
