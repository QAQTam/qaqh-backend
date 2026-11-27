//! `iroh` 1.5 本机回环传输后端（仅测试矩阵使用，`ADR-0042` r5 铁律 5）。
//!
//! - 仅以 `dev-dependency` 引入，沿用 `ADR-0039` `tls-aws-lc-rs` 单一 provider；
//! - 使用 `presets::Minimal` + `127.0.0.1:0` 绑定（零 relay、零 DNS、零外网依赖）；
//! - 实现与 `YamuxTransport` 完全同构的 [`MuxTransport`]，六大守卫同体参数化双跑。

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use iroh::Endpoint;
use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream, VarInt, presets};
use qaqh_ringing_v3::RINGING_V3_ALPN;
use qaqh_v3_transport::{MuxError, MuxTransport};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// 把 `iroh` 一对 `(SendStream, RecvStream)` 粘成统一的双向流。
pub(crate) struct IrohBiStream {
    send: SendStream,
    recv: RecvStream,
}

impl AsyncRead for IrohBiStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for IrohBiStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().send)
            .poll_write(cx, buf)
            .map_err(io::Error::other)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_shutdown(cx)
    }
}

struct IrohLifetime {
    conn: Connection,
    endpoint: Option<Endpoint>,
}

impl Drop for IrohLifetime {
    fn drop(&mut self) {
        self.conn.close(VarInt::from_u32(0), b"closed");
        if let Some(endpoint) = self.endpoint.take()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                endpoint.close().await;
            });
        }
    }
}

/// `iroh` 回环后端（可克隆句柄；最后一个克隆销毁即关闭底层 QUIC 连接）。
#[derive(Clone)]
pub(crate) struct IrohTransport {
    conn: Connection,
    _lifetime: Arc<IrohLifetime>,
}

impl IrohTransport {
    fn new(conn: Connection, endpoint: Endpoint) -> Self {
        Self {
            conn: conn.clone(),
            _lifetime: Arc::new(IrohLifetime {
                conn,
                endpoint: Some(endpoint),
            }),
        }
    }
}

fn map_conn_error(error: ConnectionError) -> MuxError {
    match error {
        ConnectionError::LocallyClosed
        | ConnectionError::ApplicationClosed(_)
        | ConnectionError::ConnectionClosed(_)
        | ConnectionError::TimedOut
        | ConnectionError::Reset => MuxError::Closed,
        other => MuxError::Protocol {
            detail: other.to_string(),
        },
    }
}

impl MuxTransport for IrohTransport {
    type BiStream = IrohBiStream;
    type UniSend = SendStream;
    type UniRecv = RecvStream;

    async fn open_bi(&self) -> Result<Self::BiStream, MuxError> {
        let (send, recv) = self.conn.open_bi().await.map_err(map_conn_error)?;
        Ok(IrohBiStream { send, recv })
    }

    async fn accept_bi(&self) -> Result<Self::BiStream, MuxError> {
        let (send, recv) = self.conn.accept_bi().await.map_err(map_conn_error)?;
        Ok(IrohBiStream { send, recv })
    }

    async fn open_uni(&self) -> Result<Self::UniSend, MuxError> {
        self.conn.open_uni().await.map_err(map_conn_error)
    }

    async fn accept_uni(&self) -> Result<Self::UniRecv, MuxError> {
        self.conn.accept_uni().await.map_err(map_conn_error)
    }
}

/// 在 `127.0.0.1:0` 建立一对已握手完毕的 `iroh` 回环连接 `(client, server)`。
pub(crate) async fn iroh_loopback_pair() -> (IrohTransport, IrohTransport) {
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let server_ep = Endpoint::builder(presets::Minimal)
        .alpns(vec![RINGING_V3_ALPN.to_vec()])
        .bind_addr(loopback)
        .expect("server bind_addr")
        .bind()
        .await
        .expect("bind iroh server endpoint");
    let client_ep = Endpoint::builder(presets::Minimal)
        .bind_addr(loopback)
        .expect("client bind_addr")
        .bind()
        .await
        .expect("bind iroh client endpoint");

    let server_addr = server_ep.addr();
    let accept_fut = async move {
        let incoming = server_ep.accept().await.expect("iroh incoming");
        let conn = incoming.await.expect("iroh server accept");
        (server_ep, conn)
    };
    let connect_fut = client_ep.connect(server_addr, RINGING_V3_ALPN);

    let (client_conn_res, (server_ep, server_conn)) = tokio::join!(connect_fut, accept_fut);
    let client_conn = client_conn_res.expect("iroh loopback connect");
    (
        IrohTransport::new(client_conn, client_ep),
        IrohTransport::new(server_conn, server_ep),
    )
}
