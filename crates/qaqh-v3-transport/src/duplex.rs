//! 内存双工闭环：不碰 socket 也能把四条逻辑流跑满。
//!
//! `tokio::io::duplex` 是**真**字节管道（同一套读写语义、同一套背压/EOF 行为，
//! 只是缓冲区在内存里），所以跑在它上面的 `yamux` 与跑在 UDS/NamedPipe 上的
//! `yamux` 是同一份代码路径。这让六大不变量守卫可以在 CI 与开发机上无端口、
//! 无权限、无残留 socket 文件地反复跑——而不是"线上才第一次真连"。

use crate::mux::MuxConfig;
use crate::mux::MuxRole;
use crate::yamux_backend::YamuxTransport;

/// 闭环默认管道容量：64 KiB 足够让四条流互相交错，又不至于把"背压"测成"无限缓冲"。
pub const DEFAULT_DUPLEX_BUFFER: usize = 64 * 1024;

/// 在同一进程内建一对已连接的多路复用端点。
///
/// 返回 `(client, server)`；两端各自 spawn 自己的 driver task，**必须**在 tokio
/// 运行期内调用。
pub fn duplex_pair(config: MuxConfig) -> (YamuxTransport, YamuxTransport) {
    duplex_pair_with_buffer(config, DEFAULT_DUPLEX_BUFFER)
}

/// 同上，但显式指定管道容量（守卫用它把"背压"压到最紧）。
pub fn duplex_pair_with_buffer(
    config: MuxConfig,
    buffer_bytes: usize,
) -> (YamuxTransport, YamuxTransport) {
    let (client_io, server_io) = tokio::io::duplex(buffer_bytes);
    let client = YamuxTransport::new(client_io, MuxRole::Client, config);
    let server = YamuxTransport::new(server_io, MuxRole::Server, config);
    (client, server)
}
