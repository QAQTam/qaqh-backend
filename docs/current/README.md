# Current Docs

> 日期：2026-09-25
> 基线：`2.0.0-alpha2`
> 状态：当前权威文档集

## 当前文档

| 文档 | 用途 |
|---|---|
| [`architecture.md`](./architecture.md) | 当前代码分层、存储、协议与运行时职责 |
| [`decisions.md`](./decisions.md) | 已冻结、不得随意回退的架构决策 |
| [`status.md`](./status.md) | 当前完成度、门禁基线与收工判断 |
| [`debug-backlog.md`](./debug-backlog.md) | 当前未完成事项、debug 优先级与延后项 |
| [`handoff/README.md`](./handoff/README.md) | 新交接文档规则 |
| [`spec/README.md`](./spec/README.md) | 新 spec 规则 |

## 维护规则

- 任何文档声称“已完成”必须给出代码路径或测试/实机证据。
- 任何文档声称“待办”必须写清优先级、阻塞关系和是否需要产品裁决。
- 旧 handoff 不作为当前依据；如需继承，先在当前文档里重新裁决。
