//! 传输层自证：闭环跑满、单向流如实拒绝、活流计数回收、句柄全灭即真断联。
//!
//! 这里刻意**不测**"单条流能搬多少字节"——那是带宽测试，不是正确性。
//! 测的是让上层敢依赖的四条性质：多流互不串扰、上限按活流算、断联可观测、能力差异显式。
//!
//! ## `yamux` 的 SYN 挂在第一帧上
//!
//! `open_bi` 返回的流在**写出第一批字节之前对端不可见**（`yamux` 把 SYN 与首个
//! 数据帧合并）。这不是缺陷而是规范行为，但它是一条必须写进契约的使用前提：
//! 上层开流后必须**立刻写首帧**（V3 就是 `StreamKind`），否则对端永远 accept 不到。
//! 下面每个用例都遵守这条前提——违反它的用例会以"accept 永不返回"的形式挂住。

use std::sync::atomic::Ordering;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::duplex::{duplex_pair, duplex_pair_with_buffer};
use crate::mux::{MuxConfig, MuxError, MuxStats, MuxTransport};
use crate::yamux_backend::{MuxStream, YamuxTransport};

/// 定长消息：`u32_le(len) ++ payload`（与 V3 帧形状同构，只为让测试像真协议）。
async fn write_message(stream: &mut MuxStream, payload: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(payload.len() as u32).to_le_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

async fn read_message(stream: &mut MuxStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

/// 服务端：来一条流就回一条同样的消息。
async fn spawn_echo_acceptor(server: YamuxTransport) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut workers = Vec::new();
        while let Ok(mut stream) = server.accept_bi().await {
            workers.push(tokio::spawn(async move {
                while let Ok(payload) = read_message(&mut stream).await {
                    if write_message(&mut stream, &payload).await.is_err() {
                        break;
                    }
                }
            }));
        }
    })
}

#[tokio::test]
async fn four_bi_streams_are_independent_and_bidirectional() {
    let (client, server) = duplex_pair(MuxConfig::default());
    let acceptor = spawn_echo_acceptor(server).await;

    // 四条流一次全开：这条断言守的是"多路复用真的在复用"，而不是"一次一条"。
    let mut streams = Vec::new();
    for index in 0..4 {
        let mut stream = client.open_bi().await.expect("open_bi");
        // 首帧必须立刻写（见模块头注：SYN 挂在第一帧上）。
        write_message(&mut stream, format!("hello-{index}").as_bytes())
            .await
            .expect("首帧");
        streams.push(stream);
    }
    for (index, stream) in streams.iter_mut().enumerate() {
        let echoed = read_message(stream).await.expect("read");
        assert_eq!(
            echoed,
            format!("hello-{index}").as_bytes(),
            "第 {index} 条流读到了别人的字节（串流）"
        );
    }

    // 交错往返：四条流同时活跃，每条都保持自己的字节序。
    for round in 0..3u32 {
        for (index, stream) in streams.iter_mut().enumerate() {
            write_message(stream, format!("r{round}-s{index}").as_bytes())
                .await
                .expect("write");
        }
        for (index, stream) in streams.iter_mut().enumerate() {
            let echoed = read_message(stream).await.expect("read");
            assert_eq!(echoed, format!("r{round}-s{index}").as_bytes());
        }
    }

    drop(streams);
    acceptor.abort();
}

#[tokio::test]
async fn unidirectional_streams_are_rejected_not_simulated() {
    let (client, _server) = duplex_pair(MuxConfig::default());
    assert!(matches!(
        client.open_uni().await,
        Err(MuxError::Unsupported { .. })
    ));
    assert!(matches!(
        client.accept_uni().await,
        Err(MuxError::Unsupported { .. })
    ));
}

#[tokio::test]
async fn live_stream_count_is_released_on_drop() {
    // 上限 2：先占满两条、全部丢弃，再开第三条——累计计数会在这里假红。
    let config = MuxConfig {
        max_inbound_streams: 2,
        accept_queue_capacity: 2,
        ..MuxConfig::default()
    };
    let (client, server) = duplex_pair(config);

    let mut first = client.open_bi().await.expect("open_bi 1");
    first.write_all(b"a").await.expect("首帧");
    let mut second = client.open_bi().await.expect("open_bi 2");
    second.write_all(b"b").await.expect("首帧");
    let accepted_first = server.accept_bi().await.expect("accept 1");
    let accepted_second = server.accept_bi().await.expect("accept 2");
    assert_eq!(server.stats().inbound_live.load(Ordering::Relaxed), 2);

    drop(accepted_first);
    drop(accepted_second);
    assert_eq!(
        server.stats().inbound_live.load(Ordering::Relaxed),
        0,
        "丢弃入站流后活流计数必须归零（否则上限会变成永久封锁）"
    );

    // 第三条流必须能进（计数器回收了才会进）。
    let mut third = client.open_bi().await.expect("open_bi 3");
    third.write_all(b"c").await.expect("首帧");
    let _accepted_third = server.accept_bi().await.expect("accept 3");
    assert_eq!(
        server.stats().inbound_rejected_limit.load(Ordering::Relaxed),
        0
    );
    drop((first, second, third));
}

#[tokio::test]
async fn inbound_limit_rejects_instead_of_parking_the_peer() {
    let config = MuxConfig {
        max_inbound_streams: 1,
        accept_queue_capacity: 1,
        ..MuxConfig::default()
    };
    let (client, server) = duplex_pair(config);

    // 对方连开三条：第一条被接走（活流 1），后两条撞上限被拒。
    let mut first = client.open_bi().await.expect("open 1");
    first.write_all(b"1").await.expect("首帧");
    let mut second = client.open_bi().await.expect("open 2");
    second.write_all(b"2").await.expect("首帧");
    let mut third = client.open_bi().await.expect("open 3");
    third.write_all(b"3").await.expect("首帧");
    let _accepted = server.accept_bi().await.expect("accept 1");

    // 别用固定 sleep 掩盖竞态：轮询到计数出现为止。
    let stats: &MuxStats = server.stats();
    for _ in 0..500 {
        if stats.inbound_rejected_limit.load(Ordering::Relaxed)
            + stats.inbound_rejected_queue_full.load(Ordering::Relaxed)
            >= 2
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        stats.inbound_rejected_limit.load(Ordering::Relaxed)
            + stats.inbound_rejected_queue_full.load(Ordering::Relaxed)
            >= 2,
        "超上限的流必须被拒并计数（当前 limit={} queue_full={}）",
        stats.inbound_rejected_limit.load(Ordering::Relaxed),
        stats.inbound_rejected_queue_full.load(Ordering::Relaxed)
    );
    drop((first, second, third));
}

#[tokio::test]
async fn dropping_every_handle_closes_the_peer_socket() {
    // 小缓冲 + 一对端点：客户端句柄全丢弃后，服务端必须看到 EOF，
    // 而不是"driver 还抱着 socket，对端永远等不到断联"。
    let (client, server) = duplex_pair_with_buffer(MuxConfig::default(), 8 * 1024);
    let mut stream = client.open_bi().await.expect("open_bi");
    stream.write_all(b"bye").await.expect("首帧");
    let accepted = server.accept_bi().await.expect("accept_bi");
    drop((stream, client));

    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), server.accept_bi())
        .await
        .expect("断联后服务端必须被唤醒（超时说明 driver 抱着 fd 不放）");
    assert!(
        matches!(closed, Err(MuxError::Closed)),
        "句柄全灭后服务端 accept 必须返回 Closed，实际 {closed:?}"
    );
    drop(accepted);
}
