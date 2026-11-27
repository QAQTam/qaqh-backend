//! 本机 IPC 监听与连接：OS 原生字节流在进入 `yamux` 前完成安全校验。

use std::io;
use std::path::Path;

use crate::YamuxTransport;

#[cfg(unix)]
use crate::unix_ipc::{PlatformListener, bind_platform, connect_platform};
#[cfg(windows)]
use crate::windows_ipc::{PlatformListener, bind_platform, connect_platform};

/// 本机 IPC 监听器；每次接受返回一条已通过 OS 身份校验的 `yamux` 服务端连接。
pub struct LocalIpcListener {
    inner: PlatformListener,
}

impl LocalIpcListener {
    /// 接受下一条本机连接。非 socket 残留路径在绑定时即拒绝，不会在这里静默修复。
    pub async fn accept(&mut self) -> io::Result<YamuxTransport> {
        let stream = self.inner.accept().await?;
        Ok(YamuxTransport::new(
            stream,
            crate::MuxRole::Server,
            crate::MuxConfig::default(),
        ))
    }
}

/// 绑定本机 IPC 端点并应用平台专属的 Owner-Only 权限。
pub async fn bind_local_ipc(path: &Path) -> io::Result<LocalIpcListener> {
    Ok(LocalIpcListener {
        inner: bind_platform(path).await?,
    })
}

/// 连接本机 IPC 端点，并在连接成功后包装为 `yamux` 客户端。
pub async fn connect_local_ipc(path: &Path) -> io::Result<YamuxTransport> {
    let stream = connect_platform(path).await?;
    Ok(YamuxTransport::new(
        stream,
        crate::MuxRole::Client,
        crate::MuxConfig::default(),
    ))
}
