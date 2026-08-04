# 后端数据流优化（backend-dataflow）

> 定位：DeepX 桌面端（Electron + SolidJS 2.0）流式数据管线的**协议契约**与**优化计划**。
> 本文件夹是"前后端数据流协同"的唯一权威参照——后端发射端与前端 reducer 共同遵守。

## 为什么存在

流式数据管线经多轮演进（legacy 协议 → Ringing v1 → 前端五轮重构），存在三类遗留问题：

1. **增量粒度**：`text_delta` 每 token、`tool_progress` 每 chunk，前端长期维护拼接状态（乱序缓冲/重放兜底）；
2. **O(N²) 快照恢复**：长会话全量快照的序列化/传输/克隆成本（首日遗留）；
3. **责任悬空**：`paced_emitter.rs` 决策"渲染器负责帧级合并"，但前端从未实现帧级合并。

本文件夹固化解法所需的两个锚点：**协议语义契约**（protocol-anchor.md）与**优化计划**（optimization-plan.md）。

## 文档索引

| 文档 | 内容 | 读者 |
|---|---|---|
| [protocol-anchor.md](./protocol-anchor.md) | 事件协议语义契约：事件分类（replaceable/增量）、发射时机、幂等/恢复语义、演进规则、前端锚点（RawSessionState）不变式 | 后端引擎开发者 + 前端 reducer 维护者 |
| [optimization-plan.md](./optimization-plan.md) | 数据流优化 PLAN：考古结论、A1/A2/B1/C1/C2 分档方案、里程碑、不做清单 | 架构决策者 + 实施者 |

## 核心原则（三层隔离）

```
Ringing 事件（协议，可演进）
  → 翻译层：ringingStores reducer / applyConversationEventToStore（唯一触碰协议）
  → 领域状态：RingingStores（按事件演进）
  → 投影层：sessionPresentation（唯一产出 UI 契约）
  → 锚点：RawSessionState / TurnViewModel（组件唯一数据依赖，永不变更）
```

- **协议变更** → 只改翻译层（reducer case），锚点不动，组件零改动；
- **演进规则** → 新事件类型 + 旧事件保留 + 白名单忽略未知 → 协议永远向后兼容；
- **锚点不变式** → 由契约测试锁定（见 protocol-anchor.md 附录）。
