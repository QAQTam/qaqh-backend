//! `yamux` 本地后端（`ADR-0042` r5 §yamux 工程约束的逐条落地）。
//!
//! ## 约束一：唯一 driver 持续推进
//!
//! [`yamux::Connection`] 的所有推进方法都要 `&mut self`，所以它**只能有一个所有者**。
//! 本模块把它整个关进 driver task：外部世界只拿到 [`YamuxTransport`]（可克隆句柄），
//! 想开流就投递一个 oneshot 槽位，想接流就从队列里取。**结构上不存在第二个 driver**。
//!
//! 反过来，只要 driver 停止被 poll，整条连接就冻住——所以 driver 的 `poll_step`
//! 必须在**同一次 poll** 里把"关闭信号 / 出站开流 / 入站收流 / 连接 I/O 推进"全部轮到，
//! 不能只 await 其中一件。这也是不用 `tokio::select!` 而手写 `poll_fn` 的原因：
//! `select!` 每次只 poll 一个分支，`poll_new_outbound` 的 `Pending` 就可能因为
//! 没人继续推进入站而永远醒不来（ack backlog 满 → 等对端 ack → 等本端读 → 死锁）。
//!
//! ## 约束二：入站流上限
//!
//! 对端可以无限开流。上限分两道：**活流计数**（[`MuxConfig::max_inbound_streams`]）
//! 与**接受队列容量**（[`MuxConfig::accept_queue_capacity`]）。两道任一触发都是
//! "拒掉这条新流"，不是"排队等"——排队等于把对端的行为变成我们的内存占用。
//!
//! 活流计数靠 [`MuxStream`] 的 `Drop` 精确回收（不是累计计数：累计会让连接跑满
//! 64 条流之后再也不能接流，那种"上限"是 bug 而不是保护）。
//!
//! ## 约束三：单向流没有
//!
//! `yamux` 规范里没有单向流。这里如实 [`MuxError::Unsupported`]，不搞"用双向流模拟
//! 单向流"的假实现（那种实现会悄悄多要一条回程窗口，语义还不对）。

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use yamux::{Config as YamuxConfig, Connection, Mode};

use crate::mux::{MuxConfig, MuxError, MuxRole, MuxStats, MuxTransport};

/// 开流槽位：driver 承诺**恰好**回填一次（成功或失败）。
type OpenSlot = oneshot::Sender<Result<MuxStream, MuxError>>;

/// 流方向（只为活流计数分桶）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Outbound,
    Inbound,
}

/// 活流守卫：流被丢弃时精确减一。
///
/// 这是"入站流上限"能成立的全部秘密：上限必须按**存活**流算。累计计数是最容易
/// 写错、也最容易被误当成"保护"的版本——它会把连接跑死在 64 条流之后。
#[derive(Debug)]
struct LiveGuard {
    stats: Arc<MuxStats>,
    flow: Flow,
}

impl LiveGuard {
    fn acquire(stats: Arc<MuxStats>, flow: Flow) -> Self {
        match flow {
            Flow::Inbound => {
                stats.inbound_live.fetch_add(1, Ordering::Relaxed);
            }
            Flow::Outbound => {
                stats.outbound_live.fetch_add(1, Ordering::Relaxed);
            }
        }
        Self { stats, flow }
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        match self.flow {
            Flow::Inbound => {
                self.stats.inbound_live.fetch_sub(1, Ordering::Relaxed);
            }
            Flow::Outbound => {
                self.stats.outbound_live.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// 一条多路复用流（双向）。
///
/// 对上层只暴露 `tokio::io::{AsyncRead, AsyncWrite}`——底下是 futures-io 的
/// `yamux::Stream`，适配只在本文件发生一次。
#[derive(Debug)]
pub struct MuxStream {
    inner: Compat<yamux::Stream>,
    _guard: LiveGuard,
}

impl AsyncRead for MuxStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }
}

/// 句柄生命周期哨兵：最后一个 [`YamuxTransport`] 克隆被丢弃时通知 driver 收摊。
///
/// 没有这一环就会出现最恶心的假活死连接：句柄没了、driver 还抱着 socket，
/// 对端永远等不到 EOF，"断联不杀进程"的守卫也就永远测不出真断联。
#[derive(Debug)]
struct Lifetime {
    shutdown: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}

impl Drop for Lifetime {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.shutdown.lock()
            && let Some(sender) = slot.take()
        {
            // driver 已经退出时 receiver 已 drop，send 失败是正常收摊，不是错误。
            let _ = sender.send(());
        }
    }
}

/// `yamux` 后端：可克隆句柄，真实连接只活在 driver task 里。
#[derive(Clone)]
pub struct YamuxTransport {
    open_bi_tx: mpsc::UnboundedSender<OpenSlot>,
    accept_rx: Arc<AsyncMutex<mpsc::Receiver<MuxStream>>>,
    stats: Arc<MuxStats>,
    /// 只为 `Drop` 而持有：最后一个克隆消失即通知 driver 收摊（故不读）。
    _lifetime: Arc<Lifetime>,
}

impl std::fmt::Debug for YamuxTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("YamuxTransport")
            .field("inbound_live", &self.stats.inbound_live.load(Ordering::Relaxed))
            .field(
                "outbound_live",
                &self.stats.outbound_live.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl YamuxTransport {
    /// 在给定 I/O 上启动一条多路复用连接（立即 spawn driver task）。
    ///
    /// `io` 是**已建立**的字节流：本地是 `UnixStream`/`NamedPipe`，测试里是
    /// `tokio::io::duplex` 的一端。本函数不做握手也不做鉴权——那些属于协议层。
    pub fn new<T>(io: T, role: MuxRole, config: MuxConfig) -> Self
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let mut yamux_config = YamuxConfig::default();
        yamux_config
            .set_max_num_streams(config.max_streams)
            .set_max_connection_receive_window(Some(config.max_connection_receive_window))
            .set_split_send_size(config.split_send_size);
        let mode = match role {
            MuxRole::Client => Mode::Client,
            MuxRole::Server => Mode::Server,
        };
        let connection = Connection::new(io.compat(), yamux_config, mode);

        let (open_bi_tx, open_bi_rx) = mpsc::unbounded_channel();
        let (accept_tx, accept_rx) = mpsc::channel(config.accept_queue_capacity.max(1));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let stats = Arc::new(MuxStats::default());

        let driver = Driver {
            connection,
            open_bi_rx,
            accept_tx,
            shutdown: shutdown_rx,
            deferred: None,
            stats: Arc::clone(&stats),
            config,
        };
        tokio::spawn(async move {
            match driver.run().await {
                Ok(()) => log::debug!("v3-mux: driver 正常收摊"),
                Err(error) => log::debug!("v3-mux: driver 退出：{error}"),
            }
        });

        Self {
            open_bi_tx,
            accept_rx: Arc::new(AsyncMutex::new(accept_rx)),
            stats,
            _lifetime: Arc::new(Lifetime {
                shutdown: std::sync::Mutex::new(Some(shutdown_tx)),
            }),
        }
    }

    /// 传输计数（守卫测试与线上诊断读取）。
    pub fn stats(&self) -> &Arc<MuxStats> {
        &self.stats
    }
}

impl MuxTransport for YamuxTransport {
    type BiStream = MuxStream;
    type UniSend = MuxStream;
    type UniRecv = MuxStream;

    async fn open_bi(&self) -> Result<Self::BiStream, MuxError> {
        let (slot, answer) = oneshot::channel();
        self.open_bi_tx.send(slot).map_err(|_| MuxError::Closed)?;
        answer.await.map_err(|_| MuxError::Closed)?
    }

    async fn accept_bi(&self) -> Result<Self::BiStream, MuxError> {
        // 单接受者语义（见 trait 文档）：并发 accept 会排队，而不是抢同一条流。
        let mut queue = self.accept_rx.lock().await;
        queue.recv().await.ok_or(MuxError::Closed)
    }

    async fn open_uni(&self) -> Result<Self::UniSend, MuxError> {
        Err(MuxError::Unsupported {
            feature: "yamux 无单向流（open_uni）",
        })
    }

    async fn accept_uni(&self) -> Result<Self::UniRecv, MuxError> {
        Err(MuxError::Unsupported {
            feature: "yamux 无单向流（accept_uni）",
        })
    }
}

/// 唯一 driver：持有 `Connection`，在**一次 poll** 里轮遍四件事。
struct Driver<T> {
    connection: Connection<Compat<T>>,
    open_bi_rx: mpsc::UnboundedReceiver<OpenSlot>,
    accept_tx: mpsc::Sender<MuxStream>,
    shutdown: oneshot::Receiver<()>,
    /// 已被上层请求、但 `poll_new_outbound` 当次给不出（ack backlog 满）的槽位。
    deferred: Option<OpenSlot>,
    stats: Arc<MuxStats>,
    config: MuxConfig,
}

impl<T> Driver<T>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    async fn run(mut self) -> Result<(), MuxError> {
        std::future::poll_fn(|cx| self.poll_step(cx)).await
    }

    fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), MuxError>> {
        // ① 句柄全没了 → 主动收摊（drop Connection 即关 io，对端立刻见 EOF）。
        if Pin::new(&mut self.shutdown).poll(cx).is_ready() {
            return Poll::Ready(Ok(()));
        }

        // ② 出站开流：一次 poll 内尽可能多开，但遇到 Pending 立刻停手，
        //    把槽位留着等下次唤醒（绝不 await 在这里，否则连接停止推进）。
        loop {
            if self.deferred.is_none() {
                match self.open_bi_rx.poll_recv(cx) {
                    Poll::Ready(Some(slot)) => self.deferred = Some(slot),
                    // 所有句柄都没了：不再有新开流请求，但连接照旧驱动到 EOF。
                    Poll::Ready(None) | Poll::Pending => {}
                }
            }
            let Some(slot) = self.deferred.take() else {
                break;
            };
            match Pin::new(&mut self.connection).poll_new_outbound(cx) {
                Poll::Ready(Ok(stream)) => {
                    self.stats.outbound_opened.fetch_add(1, Ordering::Relaxed);
                    let guard = LiveGuard::acquire(Arc::clone(&self.stats), Flow::Outbound);
                    // 槽位对面被丢弃（调用方取消）不算错误：流落地即关，guard 也随之回收。
                    let _ = slot.send(Ok(MuxStream {
                        inner: stream.compat(),
                        _guard: guard,
                    }));
                }
                Poll::Ready(Err(yamux::ConnectionError::TooManyStreams)) => {
                    // 局部流数达上限只拒该次开流请求，不能连累同一连接上其它存活流。
                    let _ = slot.send(Err(MuxError::Open {
                        detail: "出站并发流数达上限".to_string(),
                    }));
                }
                Poll::Ready(Err(error)) => {
                    let detail = error.to_string();
                    let _ = slot.send(Err(MuxError::Protocol {
                        detail: detail.clone(),
                    }));
                    return Poll::Ready(Err(MuxError::Protocol { detail }));
                }
                Poll::Pending => {
                    self.deferred = Some(slot);
                    break;
                }
            }
        }

        // ③ 入站收流 + 连接 I/O 推进（`poll_next_inbound` 就是整条连接的进度泵）。
        loop {
            match Pin::new(&mut self.connection).poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(stream))) => self.deliver(stream),
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Err(MuxError::Protocol {
                        detail: error.to_string(),
                    }));
                }
                // 对端正常关闭：driver 下班（已有流继续读到各自 EOF）。
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// 投递一条入站流；两道上限任一触发都**拒流**（不是排队等对端）。
    fn deliver(&self, stream: yamux::Stream) {
        let live = self.stats.inbound_live.load(Ordering::Relaxed);
        if live >= self.config.max_inbound_streams as u64 {
            self.stats
                .inbound_rejected_limit
                .fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "v3-mux: 入站活流达上限 {}（拒新流）",
                self.config.max_inbound_streams
            );
            drop(stream);
            return;
        }
        let guard = LiveGuard::acquire(Arc::clone(&self.stats), Flow::Inbound);
        match self.accept_tx.try_send(MuxStream {
            inner: stream.compat(),
            _guard: guard,
        }) {
            Ok(()) => {
                self.stats.inbound_accepted.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                self.stats
                    .inbound_rejected_queue_full
                    .fetch_add(1, Ordering::Relaxed);
                log::warn!("v3-mux: 接受队列已满（拒新流）：{error}");
            }
        }
    }
}
