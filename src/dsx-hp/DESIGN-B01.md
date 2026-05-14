# dsx-hp 守护核心设计文档

> **作者**: Team B - B01 (守护核心)
> **日期**: 2026-05-14
> **状态**: Phase 1 实现完成 (types/registry/pipeline/ipc_traits/main.rs 已提交)

---

## 1. 现有代码分析

### 1.1 文件清单与状态 (15 文件)

| 文件 | 行数 | 状态 | 说明 |
|------|------|------|------|
| `mod.rs` | 40 | **需重写** | 当前是 API 层(StreamEvent+client/converter), 完全不属于 hp |
| `liveness.rs` | 129 | **可复用** | 心跳+崩溃检测, 含测试, 质量好 |
| `platform.rs` | 416 | **需拆分** | DsAgentsHealthPlatform 巨型结构体(35+字段), 上帝对象 |
| `monitor.rs` | 303 | **可复用** | 实时行为监控(工具循环检测+故障阈值), 含测试 |
| `circuit_breaker.rs` | 253 | **可复用** | 熔断+工具循环检测, 含测试 |
| `sentinel.rs` | 133 | **需迁移** | API 错误分类+重试策略, 应移至 API 层 |
| `gate.rs` | 220 | **需迁移** | 消息轮换校验, 属 conversation 层 |
| `emotion.rs` | 55 | **可复用** | AgentEmotion 枚举, 简单独立 |
| `assessment.rs` | 307 | **需重写** | 在 DsAgentsHealthPlatform 上 impl, 耦合严重 |
| `quality.rs` | 218 | **需重写** | 混用 free fn 和 DsAgentsHealthPlatform impl |
| `report.rs` | 121 | **需重写** | 在 DsAgentsHealthPlatform 上 impl, 含健康 tool |
| `audit.rs` | 149 | **需迁移** | 会话完整性检查, 属 conversation 层 |
| `checks.rs` | 38 | **删除** | 是 gate.rs/audit.rs 中 check_alternation 的重复 |
| `client.rs` | ~570 | **不属于 hp** | HTTP 客户端, 应移至 API 层 |
| `converter.rs` | ~180 | **不属于 hp** | 格式转换, 应移至 API 层 |

### 1.2 核心问题

**问题 A: DsAgentsHealthPlatform 上帝对象 (platform.rs)**

包含 7 个领域 35+ 字段, 全部耦合在一个结构体里。5 个文件在其上附加 impl 块:
assessment.rs, quality.rs, report.rs, checks.rs, 以及 platform.rs 自身。

```
DsAgentsHealthPlatform
├── 模型健康   (reasoning_sample, content_buffer, tool_trail, tool_loop_count)
├── 工具链健康 (tool_failures_this_turn, tool_calls_this_turn, consecutive_failures, ...)
├── 错误台账   (recent_errors, error_counts, last_10_success)
├── 上下文健康 (context_pressure, context_tier, context_tokens, context_limit, empty_responses)
├── API 健康  (api_retries, api_errors, last_stop_reason, transport_errors)
├── Exec 健康 (exec_pending, exec_timeouts, sudo_failures)
├── 信任评分   (trust_score, consecutive_intent_misses, intent_misses_total, trust_tier)
├── 熔断器     (tool_fail_counts, disabled_tools)
├── 软限流     (tool_call_counts, throttle_warnings)
├── 资源账本   (tokens_by_category, last_successful_model, api_retry_backoff)
├── 外部健康   (disk_free_bytes, last_health_ping)
└── 聚合状态   (overall_level, turn)
```

**问题 B: 数据重复**

`platform.rs` 和 `monitor.rs` 重叠 10 个字段:
tool_calls_this_turn, tool_failures_this_turn, consecutive_failures,
consecutive_tool_only_turns, tool_fail_counts, disabled_tools,
tool_trail, tool_loop_count, reasoning_sample, content_buffer

**问题 C: 领域混淆**

- sentinel.rs (API 错误分类+重试) — 属于 API 客户端层
- gate.rs (消息轮换校验) — 属于 conversation 层
- audit.rs (会话修复) — 属于 conversation 层
- checks.rs — 是 gate/audit 的重复

---

## 2. 目标架构

### 2.1 dsx-hp 核心职责

```
dsx-hp 守护进程
├── 进程注册表   —— Agent/Tools/TUI 在线状态、心跳超时
├── 健康判决管线 —— judge() 优先级链, 输出 Verdict
└── IPC 暴露接口 —— 给 TCP IPC 层调用的 trait
```

**不属于 hp 的职责** (需迁出):

- API 错误分类+重试策略 → dsx-agent/orchestrator/
- 消息轮换校验 → dsx-agent/conversation/
- 会话修复 → dsx-agent/conversation/
- HTTP 客户端 → dsx-agent/client/

### 2.2 目标模块结构

```
src/dsx-hp/
├── mod.rs             # 模块入口, 发布类型重导出
├── types.rs           # [新] hp 内部类型: Verdict, HpError, ProcessKind
├── registry.rs        # [新] 进程注册表: 注册+心跳+超时GC
├── liveness.rs        # [保留] 单进程心跳检测 (已有, 复用)
├── monitor.rs         # [保留] 实时行为监控 (已有, 复用)
├── circuit_breaker.rs # [保留] 熔断+工具循环 (已有, 复用)
├── pipeline.rs        # [新] judge() 优先级链
├── ipc_traits.rs      # [新] 对 IPC 层暴露的 trait
└── emotion.rs         # [保留] AgentEmotion 枚举 (已有, 复用)
```

### 2.3 核心数据结构

```rust
/// Verdict — judge() 的最终输出, 给 IPC 层序列化为 JSON-LP 帧
pub enum Verdict {
    /// 一切正常
    Healthy {
        level: HealthLevel,
        emotion: AgentEmotion,
    },
    /// 降级运行
    Degraded {
        level: HealthLevel,
        reasons: Vec<String>,
        advice: String,
    },
    /// 需要干预
    Alert {
        severity: AlertSeverity,
        source: AlertSource,
        message: String,
        payload: Option<serde_json::Value>,
    },
    /// 进程死亡
    Dead {
        pid: u32,
        name: String,
        reason: String,
        since_secs: u64,
    },
}

/// 进程注册条目
pub struct Registration {
    pub pid: u32,
    pub kind: ProcessKind,
    pub name: String,
    pub started_at: Instant,
    pub liveness: LivenessState,
    pub metadata: HashMap<String, String>,
}

/// 进程注册表
pub struct ProcessRegistry {
    entries: HashMap<u32, Registration>,
    default_timeout_secs: u64,
}
```

### 2.4 judge() 判决管线

```
judge(&self, registry: &ProcessRegistry, monitor: &MonitorState) -> Vec<Verdict>

优先级链 (高→低):
├── 1. Liveness Check (最高)
│     -> 遍历 registry, 超时 → Dead Verdict
│     -> 短路: 有 Dead 直接返回
│
├── 2. Circuit Breaker Check
│     -> 检查 disabled_tools → Alert{Critical, CircuitBreaker}
│     -> check_loop → Escalate/Warn
│     -> check_failures → Escalate/Warn
│
├── 3. Monitor Check
│     -> pre_tool_gate 视角的累计状态 → Degraded 或 Alert
│
└── 4. Sentinel (来自外部, 最低)
      -> API 错误率 → Alert{Warning, Sentinel}
      -> 无问题 → Healthy
```

**短路规则**: Liveness Check 发现 Dead 直接返回, 不继续后续检查。

---

## 3. IPC 接口

### 3.1 HealthProbe trait

```rust
#[async_trait]
pub trait HealthProbe: Send + Sync {
    /// 注册进程, 开始心跳监控
    async fn register(&mut self, kind: ProcessKind, name: &str, pid: u32) -> Result<(), HpError>;
    /// 接收心跳
    async fn heartbeat(&mut self, pid: u32) -> Result<(), HpError>;
    /// 注销进程
    async fn unregister(&mut self, pid: u32) -> Result<(), HpError>;
    /// 执行完整健康判决
    async fn judge(&self) -> Vec<Verdict>;
    /// 查询特定进程健康
    async fn query(&self, pid: u32) -> Result<ProcessHealth, HpError>;
    /// 获取进程列表
    async fn list_processes(&self) -> Vec<ProcessSummary>;
}
```

### 3.2 IPC 协议映射

| IPC 请求 | HP 方法 | Verdict 类型 |
|----------|---------|-------------|
| HpRegister | register() | — |
| Heartbeat | heartbeat() | — |
| HealthQuery | judge() + query() | Healthy / Degraded / Alert |
| 心跳超时 | liveness check | Dead |

---

## 4. 复用/重写/迁移 决策

### 4.1 完全保留 (4 文件, ~740 行)

| 文件 | 保留理由 |
|------|----------|
| `liveness.rs` | 解耦良好, LivenessState + check_liveness + heartbeat, 有测试 |
| `emotion.rs` | 独立枚举, 6 变体, 无外部依赖 |
| `monitor.rs` | MonitorState + pre_tool_gate + post_tool_record, 有测试 |
| `circuit_breaker.rs` | check_loop + check_failures + track_tool_call, 有测试 |

### 4.2 需重写 (3 新文件: ~400 行)

| 文件/位置 | 内容 | 预估行数 |
|-----------|------|----------|
| `types.rs` | Verdict, HpError, ProcessKind, AlertSeverity, AlertSource | 80 |
| `registry.rs` | ProcessRegistry: register, unregister, heartbeat, gc, query | 150 |
| `pipeline.rs` | judge() 优先级链 + 辅助方法 | 120 |
| `ipc_traits.rs` | HealthProbe trait, ProcessHealth, ProcessSummary | 50 |

### 4.3 需迁移 (不属于 hp, 5 文件)

| 文件 | 目标位置 | 接收方 |
|------|----------|--------|
| `sentinel.rs` | dsx-agent/orchestrator/ | Team C |
| `gate.rs` | dsx-agent/conversation/ | Team C |
| `audit.rs` | dsx-agent/conversation/ | Team C |
| `client.rs` | dsx-agent/client/ | Team A |
| `converter.rs` | dsx-agent/converter/ | Team A |

### 4.4 删除 (1 文件)

| 文件 | 理由 |
|------|------|
| `checks.rs` | check_alternation 与 gate.rs 和 audit.rs 完全重复 |

### 4.5 解耦后依赖关系

```
mod.rs -> re-exports all pub items

pipeline.rs → registry, liveness, monitor, circuit_breaker, types
registry.rs → liveness (uses LivenessState), types
ipc_traits.rs → types (uses Verdict, HpError)
monitor.rs → circuit_breaker (uses check_circuit_breaker, record_tool_result)
circuit_breaker.rs → monitor (uses MonitorResult, MonitorState)
liveness.rs → (none)
emotion.rs → (none)
types.rs → (none)
```

关键改进:
- **原来**: 5 个文件依赖 DsAgentsHealthPlatform 上帝对象
- **现在**: pipeline 是唯一的聚合点, 组合多个独立 subsystem

---

## 5. 实现计划

### Phase 1: 新建骨架 (估算: ~300 行新代码)
1. `types.rs` — 基础枚举和错误类型
2. `registry.rs` — 进程注册表
3. `pipeline.rs` — judge() 函数
4. `ipc_traits.rs` — HealthProbe trait
5. 重写 `mod.rs` — 模块入口

### Phase 2: 适配保留代码 (无需改大量代码)
1. liveness.rs — check_liveness 直接可复用, 不需修改
2. monitor.rs — pre_tool_gate 作为 pipeline 中 Monitor Check 的内部调用
3. circuit_breaker.rs — check_loop/check_failures 直接可复用
4. emotion.rs — 纳入 Verdict 无需修改

### Phase 3: 迁移通知 + 清理
1. 通知 Team C 接收 sentinel.rs, gate.rs, audit.rs
2. 通知 Team A 接收 client.rs, converter.rs
3. Team B03 接收 quality.rs 中 check_content/check_reasoning
4. 确认迁移完成后删除 platform.rs, assessment.rs, report.rs, quality.rs, checks.rs
5. 最终 15 文件 → 8 文件, ~3600 行 → ~1200 行 (-68%)

---

## 6. 与 B02/B03 的接口

| 队友 | 交互内容 |
|------|----------|
| B02 (API 代理网关) | 通过 HealthProbe trait 调用 judge(), 在转发 API 请求前检查; B02 负责 mod.rs, 需添加 `mod types; mod registry; mod pipeline; mod ipc_traits;` |
| B03 (熔断+质量) | 接收 quality.rs 中 check_content/check_reasoning; circuit_breaker 阈值参数由 B03 定义; checks.rs 删除; audit.rs 清理 check_alternation |

---

## 7. P1/P2/P3 兼容性声明

### 影响评估

| 重构步骤 | B01 受影响？ | 说明 |
|----------|-------------|------|
| P1: ContextBuilder ← &ContextAssembler | **无** | pipeline/types 不触碰消息格式 |
| P2: convert_messages() 删除, Agent→HP 发 AnthropicMessages | **无** | 受益方是 B02 (client.rs 省掉转换); B01 模块零 `Message` 依赖 |
| P3: ToolResultAppender 单一入口 | **无** | B01 模块不涉及 ToolResult |

### 依赖追踪

B01 新文件的类型依赖:
- 外部: `serde::Serialize`, `serde::Deserialize` (JSON-LP 帧)
- 内部 hp: `liveness::LivenessState`, `emotion::AgentEmotion`, `monitor::MonitorState`
- 零依赖 `crate::types`, `crate::conversation`, `crate::api`

### 二进制目标声明

main.rs 在 `src/dsx-hp/main.rs`, 需要 Cargo.toml 添加 [[bin]]:
```toml
[[bin]]
name = "dsx-hp"
path = "src/dsx-hp/main.rs"
```

或等后续工作区重构。

---

## 8. Team B 协商后的最终站位 (2026-05-14)

### 回归 HP 的文件 (之前标记迁移, 现保留)

| 文件 | 理由 |
|------|------|
| `client.rs` | API key 隔离, HP 是唯一持有明文 key 的进程 |
| `converter.rs` | 保留 `anthropic_url()`; `convert_messages()` 由 B02 删除 |
| `sentinel.rs` | 与 client 生命周期紧密耦合, 拆开破坏内聚 |
| `gate.rs` | 留 HP 做健康审计快速路径 |

### 最终 HP 文件清单 (Phase 1 后)

```
src/dsx-hp/
├── mod.rs             # [B02 重写] 声明所有子模块
├── main.rs            # [新] HP 守护进程入口
├── types.rs           # [新] Verdict/HpError/ProcessKind/ProcessHealth
├── registry.rs        # [新] ProcessRegistry + 心跳
├── pipeline.rs        # [新] judge() 优先级链
├── ipc_traits.rs      # [新] HealthProbe trait
├── liveness.rs        # [保留] 心跳检测 + 测试
├── monitor.rs         # [保留] 实时行为监控 + 测试
├── circuit_breaker.rs # [保留] 熔断 + 测试
├── emotion.rs         # [保留] AgentEmotion 枚举
├── client.rs          # [保留, B02] HTTP 客户端
├── converter.rs       # [保留, B02] 格式转换(删 convert_messages)
├── sentinel.rs        # [保留, B02/B03] API 错误分类
├── gate.rs            # [保留, C03] 健康审计快速路径
├── quality.rs         # [待 B03 接收] check_content/check_reasoning
├── report.rs          # [待拆] exec_health + health_def
├── audit.rs           # [待 C03 接收] 私有 check_alternation 清理
├── assessment.rs      # [待删] 旧平台 impl 块, 被 pipeline 替代
├── platform.rs        # [待删] 上帝对象, 被子系统替代
└── checks.rs          # [B03 删除] 与 gate/audit 重复
```
