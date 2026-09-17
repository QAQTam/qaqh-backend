# QAQ-Harness

AI 编码代理的跨平台 **Rust 后端核心**(monorepo,16 个 workspace 成员)。单个常驻 daemon 承载多会话对话循环、LLM 网关、19 个内置工具、Agent Skills 与子代理隔离执行;Windows 桌面壳(WinUI3)/ TUI / Web 壳位于独立仓库,通过统一的 **Ringing V1** HTTP/SSE 协议接入。

- Edition 2024 · License MIT · 状态:alpha
- HTTP 栈: `axum 0.8 + hyper 1 + tower 0.5 + tower-http 0.6 + tokio 1`，`SSE KeepAlive 15s`，release 静态 CRT 单文件 exe(`opt-level=z` + LTO + strip)

## 架构总览

```
 外部壳(独立仓库):WinUI3 桌面 / TUI / Web
        │  qaqh-client(HTTP/SSE,Bearer token + client lease)
        ▼
 ┌─ qaqh-daemon ──── 常驻单实例进程(discovery: {data_dir}/daemon.json)─┐
 │   ringing_http:POST /clients/open · /commands/{control|conversation|tool} │
 │                SSE 事件三频道 + per-session timeline 流                  │
 │   QaqhService:JSON 方法分发(session.* / workspace.* / fs.* ...)          │
 │   AgentRegistry ── spawn/close ── actor(每会话一个线程,含子代理沙箱)     │
 │   RingingHub:事件双投(fanout 给所有订阅者,带 causation)                 │
 │        │                                                                 │
 │   qaqh-runtime TurnEngine:用户输入 → gate → 工具环 → 回合完成 → compact  │
 │        ├─ qaqh-gate      LLM 网关(Chat/Responses/Anthropic,SSE 流式+重试)│
 │        ├─ qaqh-workspace 19 个工具执行 + 四级权限准入 + 审计              │
 │        └─ qaqh-skills / qaqh-subagent                                     │
 └──────────────────────────────────────────────────────────────────────────┘
        │
        ├─ {data_dir}/  全局数据根(Windows: %USERPROFILE%\.qaqh;
        │               Linux/macOS: $XDG_CONFIG_HOME/qaqh(默认 ~/.config/qaqh);可用 QAQH_DATA_DIR 重定向)
        │     ├─ config.toml + secrets.toml(API key 不落明文,Windows DPAPI 加密)
        │     ├─ daemon.json / daemon.lock(发现 + 单实例锁)
        │     └─ sessions/{8位hex seed}/ meta.json · messages.jsonl · todo.json …
        └─ <workspace>/.qaqh/  项目级目录:PLAN.md · trash/ · skills/
```

## Workspace 成员

| 分层 | Crate | 职责 |
|---|---|---|
| 领域/线协议 | `qaqh-domain` | 中立 DomainCommand/DomainEvent + 回合聚合投影等共享模型（复用 `qaqh-types` 的规范工具结果模型（ContentRef/ToolResult 经 `event.rs` 重导出）） |
| | `qaqh-ringing` | Ringing 线协议:envelope / ack / batch / snapshot / content ref / worker frame / 能力协商 |
| 运行时 | `qaqh-runtime` | daemon 应用运行时:`QaqhService` 方法分发、AgentRegistry、actor、RingingHub、TurnEngine(对话循环:输入处理 → gate 快照 → 工具审批/执行 → 回合完成 → 自动压缩) |
| | `qaqh-message` | 消息存储状态机(Turn/Step 结构、Effect 驱动、ContextFlow 摄取编排) |
| | `qaqh-daemon` | headless 入口二进制(`run` / `server` / `status` / `stop`) |
| 会话/配置 | `qaqh-session` | SessionManager 单例:index/meta/消息 JSONL 持久化、归档、临时会话、WorkspaceStore |
| | `qaqh-types` | 共享类型、平台路径(data_dir/marker)、tool_mode 定义、DaemonDiscovery(daemon.json 磁盘契约唯一源) |
| | `qaqh-config` | Config 加载/保存事务、provider 注册表、system prompt、secrets |
| | `qaqh-config-api` | 配置契约层(wire DTO):ConfigDto 读模型 / ConfigPatch 写模型,多前端共享唯一真相 |
| LLM | `qaqh-gate` | LLM API 网关:OpenAI Chat Completions / Responses / Anthropic Messages 三协议、自研 SSE 解码器(~143MB/s)、429/5xx 指数退避重试、reasoning/tool-call 流提取 |
| 工具 | `qaqh-workspace` | 进程内工具执行框架 + 19 个内置工具 + 权限/审计 |
| | `qaqh-subagent` | `spawn_subagent`:派生隔离 Ringing 子会话(in-process 守护线程,ephemeral,结果异步注入父会话) |
| | `qaqh-skills` | Agent Skills 发现/解析/激活(SKILL.md + YAML frontmatter,catalog 渐进披露) |
| | `qaqh-mcp` | MCP **客户端**支持:server 连接/生命周期/冷却 + 工具与资源投影(`mcp__{server}__{tool}` 与聚合只读工具 `mcp`) |
| | `qaqh-lsp` | LSP **客户端**支持:按扩展名路由的 server 管理 + 精确代码导航(definition/references/hover/documentSymbol/workspaceSymbol) |
| 客户端/周边 | `qaqh-client` | daemon HTTP/SSE 传输层:discovery → open 协商 → 三频道 SSE + timeline 流 + lease 自愈;供外部壳复用 |

## 核心概念

### Ringing V1 协议
客户端先 `POST /clients/open` 能力协商,获得 `client_instance_id / session_id / lease`;命令按 control/conversation/tool 三频道 POST,事件经对应频道 SSE 推送(batch 信封,16MB 帧上限);另有 per-session timeline SSE(快照页 + Last-Event-ID 断点续传)。鉴权三层:Bearer token + client-session lease + seed 所有权。worker 已收敛为 daemon 内线程,但保留完整 frame 边界语义,未来可无感切回子进程隔离。

### 多前端与 webUI 托管
daemon 是唯一协议面:WinUI3 桌面壳 / Tauri / Electron / TUI / 浏览器一律以 Ringing V1 HTTP/SSE 接入,daemon 侧不存在任何第二前端协议。浏览器形态由 daemon 内置静态托管承担:`GET /debug/` 直接服务 renderer 静态产物(定位 `out/renderer`,electron-vite 布局),入口页加载 `__qaqh_bridge__.js` 获取一次性 nonce,再经同源 `/debug/__qaqh_token__` 兑换运行 token——改前端 → 刷新浏览器即可,无需重打包。安全边界:**仅限 loopback 来源**(非回环连接一律 403,LAN 模式下远端壳是已持 token 的原生应用);debug 托管本身只提供静态读取与 nonce 兑换端点。

### 会话与存储
- seed 为 8 位 hex;磁盘布局 `sessions/index.json` + `sessions/{seed}/{meta.json, messages.jsonl, compact-context.json, todo.json}`,全部 temp+rename 原子写
- 归档会话保留磁盘可恢复;**临时会话**(子代理)关闭即整目录删除,零残留
- 上下文超过 `auto_compact_threshold`(默认 context_limit × 0.75)自动摘要压缩;原始 JSONL 不可变归档,resume 走 compact-context 检查点链(fail-closed)

### 工具与权限
19 个工具分四类权限类别(Read/Write/Exec/Net),四级权限档位:

| Level | 名称 | 行为 |
|---|---|---|
| 1 | MaxLockdown | 一切调用需确认 |
| 2 | ReadFree | 读放行,写/exec/net 需确认 |
| 3 | WorkspaceFree | 工作区内写放行;跨区写一次性信任文件夹;exec/net 仍需确认（新配置默认档） |
| 4 | Unrestricted | 显式危险 bypass，普通工具全部放行；exec 沙箱待补，可越出工作区 |

- 审批闭环:`PermissionChallenge`(一次性,TTL)→ UI 确认 → 不可伪造的授权凭证执行;支持 trust folder
- 写入防漂移:read/edit/write 维护文件 hash 账本,失配报 `STALE_FILE`;dry-run 暂存 pending_id 后 `confirm_apply` 直提
- 子代理沙箱:读写自动批准,exec/net 自动拒绝,无弹窗通道
- 工具模式档位:`standard` / `minimal` / `minimal:b` / `minimal:c` / `custom`(白名单 + 模型面投影)

### Provider 与配置
内置 13 家 provider 注册表(deepseek/qwen/glm/kimi/mimo/minimax/doubao/openai/openrouter/zcode/workbuddy/deepseek-web/opencode-go),endpoint 级声明协议(openai/responses)、thinking 字段、缓存字段等能力,新 provider 只加配置不改网关代码。`config.toml` 支持命名 profiles;API key 存 `secrets.toml`(Windows DPAPI 加密,其余平台 0600 明文),config 中只留 `"set"` 标记。

### 技能系统
扫描项目 `.qaqh/skills > .agents/skills > skills`,再用户级同名目录;SKILL.md frontmatter 必填 name/description。catalog 只注入元数据(progressive disclosure),正文仅在 `$mention` 或 `skills activate` 时经类型化 effect 通道注入 `<skill_context_envelope>`;allowed-tools 永不自行授予权限。

## 快速开始

```powershell
# 构建(release,产出 daemon 二进制)
just build-daemon

# 开发运行(headless daemon)
just dev

# 手动管理 daemon
cargo run -p qaqh-daemon -- run      # 默认启动
cargo run -p qaqh-daemon -- server   # 局域网 headless 模式(远端壳直连)
cargo run -p qaqh-daemon -- status   # 读 daemon.json 探活
cargo run -p qaqh-daemon -- stop

# webUI(浏览器直连,与桌面壳同一 renderer)
# 启动 daemon 后打开 http://127.0.0.1:<port>/debug/
# (端口读 daemon.json;renderer 产物放在 out/renderer 或用
#  QAQH_DEBUG_RENDERER_DIR 指定;仅限本机访问)
```

## 开发工作流

| Recipe | 作用 |
|---|---|
| `just check` | `cargo check --workspace` |
| `just clippy` | `cargo clippy --workspace --all-targets` |
| `just fmt` | `cargo fmt --all --check` |
| `just test` | `cargo test --workspace` |
| `just build-daemon` | release 编译核心二进制 |
| `just status` / `clean` | 产物检查 / 清理(仅 Windows 的 status) |
| `just sync-version` | 从 `version.txt` 同步版本到 Cargo.toml + package.json |

- Clippy 全仓 deny `unwrap_used`、`string_slice`(少数 crate 局部豁免并注明理由;测试代码经 clippy.toml 豁免)
- 测试规模以 `cargo test --workspace -- --list` 为准:当前列出 1300+ 用例，并包含 63 个集成测试文件；触碰全局状态的测试统一走 `TEST_RUNTIME_SERIAL` 互斥串行
- 测试/多实例用 `QAQH_DATA_DIR` 环境变量整体重定向数据根
- 云端 `.cnb.yml` 只负责 NPC 审查与镜像构建;Rust test/clippy 当前不跑云端，质量门禁以本地 recipe 链为准

## 版本管理

- `version.txt` 是唯一版本真源,经 `scripts/sync-version.ps1` 写入 Cargo.toml 与 package.json
- 对外 User-Agent 版本(`QAQH_USER_AGENT = "qaqharness/<ver>/`)在 `crates/qaqh-types/src/platform.rs` 宏内手工维护,与 cargo 包版本解耦(不带 rc/预发布后缀)
