# WinUI 与 Ringing 的后续重做说明

当前 WinUI 只是承载现有 SolidJS renderer 的原生窗口壳，不拥有独立的 daemon 协议客户端；
因此本目录暂不做 Ringing 适配，也不作为 Electron Ringing 主链的验收阻塞项。

待 Electron 的 Ringing 协议、事件语义和恢复语义稳定后，再结合最终的桌面宿主边界直接重做
WinUI 集成。届时需要重新确认：

- renderer 与 WinUI 宿主之间的启动、关闭和错误恢复契约；
- Ringing 连接由 renderer/Electron 负责，还是由 WinUI 宿主负责；
- 会话事件、bootstrap、命令终态和交互提示在宿主重启后的恢复方式；
- 安装、升级和 daemon 生命周期管理是否仍由 Electron 主进程承担。

在上述语义冻结前，不复制 Electron 的临时适配，也不修改当前 WinUI 源码。
