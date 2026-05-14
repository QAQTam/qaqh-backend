# DSX v4.0 — Next Development Steps

> 2026-05-14 | 从探索到设计到隔离修改全部完成

## 当前状态

- 80 源文件按 5 bin + 2 lib 归类完毕
- 5 Team 15 人隔离修改完成
- `cargo test` 4 passed, `cargo build --release` 成功
- 88 warnings（全部 unused imports，预期内）

## Phase 4: 联动集成

### 1. Cargo Workspace 配置

将单 `Cargo.toml` 拆为 workspace：

```
dsx/
├── Cargo.toml          [workspace]
├── crates/
│   ├── dsx-types/      lib
│   ├── dsx-proto/      lib
│   ├── dsx-hp/         bin  (TCP :9001)
│   ├── dsx-agent/      bin  (stdin/stdout)
│   ├── dsx-tools/      bin  (stdin/stdout)
│   ├── dsx-tui/        bin  (ratatui)
│   └── dsx/            bin  (Tauri, 未来)
```

### 2. IPC 通道联动

- **TUI → Agent**: JSON-LP stdin/stdout
  - TuiToAgent / AgentToTui 帧
  - app.rs 移除 → state.rs + AgentConnection
- **Agent → Tools**: JSON-LP stdin/stdout
  - ToolCallReq / ToolProgress / ToolResult / tool_result_message
  - Agent spawn Tools 子进程
- **Agent → HP**: JSON-LP TCP localhost
  - ApiRequest (AnthropicMessages 直传) / HpStreamChunk / HpResponseDone
  - HP gate 校验 → POST API

### 3. 启动顺序

```
dsx-hp 启动（TCP 守护）
  → dsx-tui 启动
    → spawn dsx-agent（stdin/stdout）
      → dsx-agent spawn dsx-tools（stdin/stdout）
      → dsx-agent TCP connect dsx-hp
```

### 4. 清理

- [ ] 88 warnings（unused imports）
- [ ] 跨 crate import 路径统一（`crate::` → `dsx_types::` 等）
- [ ] legacy app.rs 完全移除
- [ ] 5 个 main.rs 各自独立编译
- [ ] convert_messages() 最终删除（HP 侧）
- [ ] 上帝对象拆分 trait 实现（HealthProbe, ToolGate, HealthAudit 等）
- [ ] StreamEvent GateBlocked/ApiError/RetryProgress/RateLimitInfo 数据流接入
- [ ] session/skills IPC stub 替换为真实实现

### 5. 未闭合的 TODO

- glance 的 semantic data → 通过 IPC 从 Agent 获取
- list_skills / read_skill_ref → 通过 IPC 从 Agent 获取
- task/plan/memory 的 session I/O → 通过 IPC 从 Agent 获取
- status 的 AtomicU8 → RouterCommand IPC
