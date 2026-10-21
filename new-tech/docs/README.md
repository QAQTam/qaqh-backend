# QAQH v2.1 → v3.0 渐进重构文档

唯一入口：[重构方案](v2-reset/README.md)。`v2-reset/` 保留原路径以稳定引用，不代表要求 v2.1 一次完成重置。

- [发布与迁移规则](v2-reset/rollout.md)：v2.1 起按功能切片发布，v3.0 收敛架构；版本、数据、兼容退出和验收边界。
- [能力保留矩阵](v2-reset/preservation.md)：当前前后端业务增量、目标所有者及不退化验收；每次施工前刷新。
- [Provider 与账号](v2-reset/providers.md)：API key、ChatGPT 订阅、DeepSeek 账号、凭据与模型能力的独立边界。
- [配置契约与当前切片](v2-reset/configuration.md)：profile 目录、详情、主机读模型及后续唯一写入边界。

- [协议](v2-reset/protocol.md)：Ringing wire major 3、可靠消费、诊断、终端。
- [消息、撤回与 fork](v2-reset/messages.md)：细粒度内容单元、服务器顺序、上下文边界与分支。
- [工具与生态](v2-reset/ecosystem.md)：公共包、市场兼容、skills/MCP/LSP 与安装生命周期。
- [组合与热插拔](v2-reset/composition.md)：资源所有权、依赖、代际准入、升级与真实停止。
- [客户端工具与 SDK](v2-reset/client-tools.md)：设备工具宿主、固定路由、断线对账和原生多语言接入。
- [ACP](v2-reset/acp.md)：编辑器接入、外部 agent 委派与终端边界。
- [agent](v2-reset/agent.md)：薄 loop、功能插件、消息和工具路径。
- [实现简化与测试取舍](v2-reset/engineering.md)：边界校验、旧补偿退出、Rust 实现规则与测试保留标准。
- [依赖矩阵](v2-reset/dependencies.json)：v3.0 目标 crate 及允许边，不是 v2.1 全量开工前置条件。
- [实现状态](ARCHITECTURE.md)：区分 again 设计工作树、当前产品代码与目标。

只维护现行契约与施工出口，不新增 bug 账本、交接日志、旧版本设计归档。
beta4 和 tools/v2-architecture 的 baseline 仅保留原审计历史，不再代表当前产品基线。
当前能力以保留矩阵记录的前后端提交及源码为依据；更改协议或 crate 边界时直接更新现行契约，保持一个结论。
