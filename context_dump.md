# DSX 开发会话上下文（Claude Code 视角）

## 会话范围
连续两个会话（2026-05-15），都在做 DSX v4.0.0-alpha1 的集成调试。

## 核心目标
DSX 从单体 binary 拆分为 5 进程架构（dsx-hp/agent/tools/tui），本会话的任务是：
- 打通端到端管道（TUI → Agent → HP → API → Tools → Response）
- 修复所有阻塞性问题（HP dispatch、死锁、400 错误、流式、工具执行）
- 支持双协议（OpenAI + Anthropic）
- 添加配置向导
- 本地模型支持
- tok/s 实时显示
- 思考链按轮次分块

## 架构
```
LM Studio ←HTTP→ HP (TCP:JSON-LP) → Agent (TCP) → TUI
                               → Tools (stdin/stdout)
```

- **dsx** (umbrella binary): 入口，支持子命令 hp/agent/tools/tui/config
- **dsx-types**: 核心类型 (Message, ToolCall, UsageInfo, etc.)
- **dsx-proto**: 6 组 IPC 帧枚举 (TuiToAgent, AgentToTui, AgentToTools, ToolsToAgent, AgentToHp, HpToAgent)
- **dsx-gateway**: 协议转换库 (Anthropic SSE + OpenAI SSE)
- **dsx-hp**: 健康守护 + API 代理网关
- **dsx-agent**: 核心编排（39 个模块：session/memory/orchestrator/context/skills/router）
- **dsx-tools**: 工具执行（10 个工具：explore/exec/read_file/write_file/edit_file/list_dir/search/glance/status/ask_user）
- **dsx-tui**: 终端界面（ratatui）

## 本会话纪要

### 之前会话完成的工作
- 5 进程管道打通（TUI→Agent→HP→API→Tools→Response）
- dsx-proto 协议定义完成（6 组帧枚举）
- dsx-gateway 协议转换（Anthropic + OpenAI）
- 双协议支持
- 配置向导（dsx --init）
- 本地模型过滤（12K context 只暴露 10 个工具）
- Auto-mode（自动接受工具确认）
- 会话隔离
- 纯静态 binary（21MB）
- /snapshot 定时器移除
- config priority 修正为 file-first

### 本次会话的工作

#### 1. LM Studio GPU 加速修复
- **问题**: LM Studio 用 AVX2（CPU）后端，显卡完全没用上
- **修复**: 将 `~/.lmstudio/.internal/backend-preferences-v1.json` 从 `llama.cpp-linux-x86_64-avx2` 改为 `llama.cpp-linux-x86_64-vulkan-avx2`
- **效果**: 2.5 tok/s → 47 tok/s（18 倍提升）
- **硬件**: AMD Radeon RX 6750 GRE 10GB (RADV NAVI22) + 32GB RAM
- **模型**: qwen/qwen3.5-9b（本地）

#### 2. System prompt 优化
- 加了"不要展示思考过程"的规则（但 Qwen 内置思考，效果有限）
- 后续改回

#### 3. tok/s 实时显示
- **文件修改**: `crates/dsx-tui/src/state.rs`, `lib.rs`, `render/workspace.rs`
- **实现**: record request_start_time → on api_response 计算 completion_tokens / elapsed → display in info panel
- 还修复了 context bar 使用真实 usage 而不是估算值

#### 4. 思考链按轮次分块
- **问题**: 所有轮次的推理链全部累加到同一个 reasoning 字段
- **修复**: StreamBuffer 增加 reasoning_blocks / tool_calls_per_block，在 ContentDelta(reasoning) 到来时检测工具进度是否为非空→ 轮次边界
- **渲染**: 每个 reasoning block 后跟对应的 tool call

#### 5. 仍存在的问题
- 思考链在 TUI 还是连起来显示（已修复但未验证）
- 本地模型还是会瞎想（内置 reasoning）
- 没有 session 持久化（每次重启新会话）
- 需要解决 max_tokens 限制

### 技术细节
- **Binary**: `target/release/dsx`, 21MB 静态链接
- **Config**: `~/.dsx/config.json`
- **LM Studio**: `~/.lmstudio/`, Vulkan backend 2.13.0
- **Streaming pipeline**: LM Studio HTTP SSE → HP (StreamEvent) → Agent (HpToAgent) → TUI (AgentToTui)
- **Frame protocol**: JSON-LP, 每帧一行 `\n` 分隔, tagged with "type"
