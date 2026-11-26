//! 极薄多路复用传输抽象（`ADR-0042` 铁律 4）。
//!
//! 全部内容就是一个 trait + 四种错误 + 一份配置。**故意不做**的事：
//!
//! - 不做"流分级/QoS"：`Reliable` / `Replaceable` 是**帧属性**，不是传输属性；
//!   传输层只负责"多条独立流共享一条连接"，语义一概不懂（铁律 4）。
//! - 不做 `dyn` 装箱：门面按具体后端**泛型静态分发**，`MuxTransport` 因此
//!   在方法上返回 `impl Future`（RPITIT）而不是 `Box<dyn Future>`——
//!   这同时让 trait 天然非对象安全，杜绝有人"临时 box 一下"把静态分发毁掉。
//! - 不做重试/重连：断联语义属于上层（断联不杀进程 + 幂等回执），
//!   传输层失联就是失联，绝不"悄悄再连一条"冒充原连接。

use std::future::Future;

use tokio::io::{AsyncRead, AsyncWrite};

/// 传输错误（全部 Fail-Closed：不做降级、不做重试）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MuxError {
    /// 连接已关闭（对端关闭或本端 driver 已退出）。
    #[error("多路复用连接已关闭")]
    Closed,
    /// 该后端不支持此项能力（`yamux` 无单向流）。
    #[error("该传输后端不支持：{feature}")]
    Unsupported {
        /// 能力名（用于日志与错误文案）。
        feature: &'static str,
    },
    /// 入站流被上限挡下（保护本端内存；对端会看到流被重置）。
    #[error("入站流达上限 {limit}，新流被拒")]
    InboundStreamLimit {
        /// 上限值。
        limit: usize,
    },
    /// 打开出站流失败。
    #[error("打开出站流失败：{detail}")]
    Open {
        /// 底层错误文案。
        detail: String,
    },
    /// 多路复用协议层错误（帧损坏、状态机非法迁移等）。
    #[error("多路复用协议错误：{detail}")]
    Protocol {
        /// 底层错误文案。
        detail: String,
    },
    /// 底层 I/O 错误。
    #[error("传输 I/O 错误：{detail}")]
    Io {
        /// 底层错误文案。
        detail: String,
    },
}

/// 多路复用角色（决定流 id 的奇偶归属，与 `iroh`/QUIC 的角色语义一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxRole {
    /// 主动发起连接的一侧。
    Client,
    /// 被动接受连接的一侧。
    Server,
}

/// 多路复用参数（`ADR-0042` r5：入站流上限 + 有界接收窗，全额可配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuxConfig {
    /// 同时存活的入站流上限（超过即拒新流，保护本端内存）。
    pub max_inbound_streams: usize,
    /// 已接受但尚未被上层取走的入站流队列容量（满了即拒，绝不无限堆积）。
    pub accept_queue_capacity: usize,
    /// 连接级接收窗口总字节数。
    ///
    /// **硬约束**：必须 `>= 256 KiB × max_streams`（`yamux` 要求每条流至少拿到默认
    /// 256 KiB 信用，违反即 panic）。这是一条**内存上界**而不是建议值：接收窗口
    /// 就是"对端最多能塞给我们多少未读数据"，调大了等于把宿主内存交给对端。
    /// 默认 16 MiB = 64 × 256 KiB，恰好卡在允许的最小值上。
    pub max_connection_receive_window: usize,
    /// 单条连接允许的流总数（含出站）。
    pub max_streams: usize,
    /// 单帧数据上限（`yamux` 用它降低跨流队头阻塞）。
    pub split_send_size: usize,
}

impl Default for MuxConfig {
    fn default() -> Self {
        Self {
            // V3 只有四类逻辑流，但一个会话可以有很多 Blob/子会话流；
            // 64 是"够用且不会把 8 GiB 宿主拖垮"的取值。
            max_inbound_streams: 64,
            // 与活流上限对齐：接受队列里的每一条也都是活流，队列容量再小就会
            // 抢在活流上限之前先拒流（两道闸口顺序反了，日志会误导排查）。
            accept_queue_capacity: 64,
            // 16 MiB = 64 × 256 KiB：既是 yamux 允许的最小值，也是我们的内存上界。
            max_connection_receive_window: 16 * 1024 * 1024,
            max_streams: 64,
            // 16 KiB 与 yamux 默认值一致：小帧才好让不同流交错，队头阻塞最小。
            split_send_size: 16 * 1024,
        }
    }
}

/// 传输计数（用于守卫断言与线上诊断；不含任何业务语义）。
#[derive(Debug, Default)]
pub struct MuxStats {
    /// 已成功打开的出站流数（累计）。
    pub outbound_opened: std::sync::atomic::AtomicU64,
    /// 已成功接受的入站流数（累计）。
    pub inbound_accepted: std::sync::atomic::AtomicU64,
    /// 当前存活的出站流数（**活流**，流 Drop 时减一）。
    pub outbound_live: std::sync::atomic::AtomicU64,
    /// 当前存活的入站流数（**活流**，入站上限就是按它判定）。
    pub inbound_live: std::sync::atomic::AtomicU64,
    /// 因入站活流超过上限被拒的流数。
    pub inbound_rejected_limit: std::sync::atomic::AtomicU64,
    /// 因接受队列已满被拒的流数。
    pub inbound_rejected_queue_full: std::sync::atomic::AtomicU64,
}

/// 极薄多路复用传输抽象。
///
/// 签名与 `v4.8` 的 `iroh::Connection` 逐字对齐：`open_bi` / `accept_bi` /
/// `open_uni` / `accept_uni`。`BiStream` 是**一条**双向字节流（不是 `(Send, Recv)`
/// 两个半边）——`yamux` 如此，`iroh` 侧在 impl 里把两个半边粘成一个 `BiStream`
/// 即可，域代码不必知道差异。
pub trait MuxTransport: Clone + Send + Sync + 'static {
    /// 双向流类型（实现 `tokio` 的 `AsyncRead + AsyncWrite`）。
    type BiStream: AsyncRead + AsyncWrite + Send + Unpin + 'static;
    /// 单向发送半边。
    type UniSend: AsyncWrite + Send + Unpin + 'static;
    /// 单向接收半边。
    type UniRecv: AsyncRead + Send + Unpin + 'static;

    /// 打开一条出站双向流（`iroh` 对齐面）。
    ///
    /// **使用前提**：拿到流后必须**立刻写首帧**，对端才看得见这条流——
    /// `yamux` 把 SYN 与第一个数据帧合并发送（规范行为，不是缺陷）。V3 的首帧
    /// 就是 `StreamKind`，所以协议层天生满足它；但任何"先开流、稍后再写"的写法
    /// 都会让对端的 `accept_bi` 永远等不到。
    fn open_bi(&self) -> impl Future<Output = Result<Self::BiStream, MuxError>> + Send;

    /// 接受一条入站双向流。
    ///
    /// **同一连接只应有一个接受循环**：入站流的分派（按首帧 `StreamKind` 路由到
    /// 四条逻辑流）天然是单点的事，多个并发 acceptor 只会把路由逻辑撕碎。
    fn accept_bi(&self) -> impl Future<Output = Result<Self::BiStream, MuxError>> + Send;

    /// 打开一条出站单向流。
    ///
    /// `yamux` 后端恒 [`MuxError::Unsupported`]（规范里没有单向流）；保留此方法是为了
    /// `v4.8` `iroh` 后端零返工，以及将来"纯服务端推送"的 `SessionEvents` 可以省掉回程。
    fn open_uni(&self) -> impl Future<Output = Result<Self::UniSend, MuxError>> + Send;

    /// 接受一条入站单向流（同上，`yamux` 恒不支持）。
    fn accept_uni(&self) -> impl Future<Output = Result<Self::UniRecv, MuxError>> + Send;
}
