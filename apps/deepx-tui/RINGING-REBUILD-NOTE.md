# TUI 与 Ringing 的后续重做说明

当前 TUI 暂不参与 Ringing v1 主链验收，也不在 Electron 协议稳定前做渐进式迁移。

当前事实：TUI 仍通过 `deepx-client::DeepxClient` 使用 `/control/v1` legacy WebSocket，
状态层仍消费 `ControlServerMessage` / `Agent2Ui`。`deepx-client` 中已有
`NativeRingingClient` 的连接骨架，但尚未具备完整 SSE、cursor、bootstrap、lease 恢复和
Ringing 事件 reducer，因此不把它视为可用的 TUI Ringing 实现。

后续启动条件：

1. Electron 的 Ringing 主链通过稳定性验证；
2. DomainEvent、command ack/terminal、bootstrap、SSE 重连和协议版本语义冻结；
3. 旧协议退役清单中的对照 fixture 和回归场景准备完成。

达到条件后，TUI 直接按最终 Ringing 契约重做客户端和状态模型，不要求保留现有 legacy
客户端的中间兼容形状。重做至少覆盖：连接协商、三频道事件、游标恢复、会话快照、命令
终态、交互响应、断线重连和错误展示。

本说明只记录边界，不改变当前 TUI 源码。
