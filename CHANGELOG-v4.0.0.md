# DSX v4.0.0-alpha1 — 大破大立

> 2026-05-14 | 80 files, 15 teammates, 0 lines untouched

## Breaking Everything

单体 binary 拆为 5 进程 + 2 共享库的 workspace 架构：

```
dsx-hp      — 健康守护进程 + API 代理网关（独立 TCP 守护，唯一持有 API key）
dsx-agent   — Agent 核心（无界面，stdin/stdout 子进程）
dsx-tools   — 工具执行服务器（沙箱隔离，stdin/stdout 子进程）
dsx-tui     — 终端界面（ratatui，父进程 spawn Agent）
dsx         — Tauri GUI（未来）

dsx-types   — 共享类型定义（35→26 类型，9 子模块）
dsx-proto   — IPC 消息协议（3 通道 JSON-LP）
```

## 为什么拆

**沙盒缺失：** v3.x exec 和 agent 在同一进程。ANSI 转义污染 TTY、授权弹窗死锁、orphan tool_use 400——全是同进程耦合导致的。gate 在被监控者地址空间内，prompt 可绕。

**HP 变上帝对象：** health.rs 通过自升级从 0→680L，累积 7 领域 35+ 字段。OnceLock→Mutex 修复暴露根因：HP 不应与 agent 共享内存。

**自升级失速：** v3.4.0 记忆系统冻结。系统诊断出"低信噪比"但无法解决——自升级只能加代码不能改架构。

## 架构决策

### PID 1 是 dsx-hp
- TCP :9001 守护，先于所有进程启动
- 唯一持有 API key，Agent 不直接调 API
- gate 在独立进程——物理不可绕
- 监控 Agent/Tools 心跳，有独立 kill 权限

### IPC 三条通道
- TUI↔Agent: stdin/stdout JSON-LP（父→子进程）
- Agent↔Tools: stdin/stdout JSON-LP（父→子进程）
- Agent↔HP: TCP localhost JSON-LP（守护进程）

### 上下文流转重构（P1/P2/P3）
- P1: ContextBuilder 直接收 `&ContextAssembler`，输出 `Vec<AnthropicMessage>`——消除 `Vec<Message>` 中间格式
- P2: HP convert_messages() 删除——AnthropicMessages 直传
- P3: ToolResultAppender 单一入口——消除 3 处分散的 push_tool

### 二级工具菜单
```
exec/{run, sudo_run, pty}
file/{read, write, edit, diff, list, search}
git/{status, diff, log, commit}
web/{fetch, search, context7_*}
meta/{list_skills, read_skill_ref, glance}
task/{create, update, list}
plan/{create, update, read, list}
memory/{save, read, forget, recall, pitfall_save, pitfall_guide}
explore        ← 独立顶级工具
status, ask_user, health
```

### Gate 两层模型
| 类型 | 域 | 例子 | 机制 |
|------|----|------|------|
| 安全 gate | HP | circuit breaker, tool loop, 连续失败 | Block（物理不可绕） |
| 工作流 gate | Agent | explore→edit, phase_check, intent | Block（dispatch 拒绝） |

### check_alternation 3→1
- checks.rs 删除（37 行死代码）
- audit.rs 私有 check_alternation 删除
- 权威实现：ContextAssembler::validate()
- gate.rs 保留 HP 快速路径

### 4 个新 StreamEvent 变体
GateBlocked / ApiError / RetryProgress / RateLimitInfo — B02+B03+C03 三方对齐，E03 TUI 展示已实现

### 多 Provider 支持
`ProviderKind` enum 配置驱动（DeepSeek / Anthropic / OpenAiCompatible），IPC 协议无需改动

### God Object 拆分
DsAgentsHealthPlatform trait 先行 → reactive pipeline → HealthVerdict

## 变更文件

- dsx-types: 9 子模块拆分 + 9 死类型删除 + token/error/serde 新增
- dsx-agent: ContextBuilder 重写 + ContextAssembler 封闭 5 绕过 + agent_loop 适配 + phase_detector 新增
- dsx-hp: main.rs 新建 + converter 删 convert_messages + client 重构 + checks.rs 删除 + gate 确认
- dsx-tools: ToolManager IPC 框架 + agent.rs 移除 + file/meta 重构 + tools 注册
- dsx-tui: app.rs → AppState/AgentConnection/StreamBuffer + InputAction+InputState + render 三层新增
- explore.rs: USER 最终裁定独立顶级工具

## 待办（Phase 4）

- Cargo workspace 配置
- IPC 通道联动（3 条）
- 5 个 main.rs 各自独立编译
- 88 warnings 清理
- legacy app.rs 移除
- session/skills IPC stub → 真实实现
