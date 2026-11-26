//! 守卫②：**多流背压隔离 + 量化延迟上界**（`ADR-0042` 第四节第 10 条）。
//!
//! ## 同时加压的三条洪峰
//!
//! 1. 会话 A 的 `Replaceable` 事件洪峰（AgentLoop 的位置）；
//! 2. Shell PTY 刷屏（4 KiB 一块，连续不断）；
//! 3. 8 MiB 大 Blob 分片推送（服务端推通道）。
//!
//! 在这个背景下量两条延迟：
//!
//! | 指标 | 红线 | 理由 |
//! |---|---|---|
//! | 控制面 RPC 往返 P99 | **< 50ms** | 审批/停止必须随时点得动 |
//! | PTY 回显往返 P99 | **≤ 16.6ms** | 终端键回显要在这一帧内到（60fps 预算） |
//! | 会话 B 帧间隔 P99 | **< 50ms** | 一个会话的洪峰不得饿死另一个会话 |
//!
//! ## 不达标就切后端，不调参
//!
//! 阈值来自计划与 `ADR-0042`，**本守卫不放宽**。未达标即触发"整体切 `iroh` 回环"
//! 的决策条件——把红线调宽会让这条守卫变成装饰品。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use qaqh_client::v3::ShellOutputStream;
use qaqh_ringing_v3::{ControlRequest, ControlResponse, SessionEvent, SessionRequest, ShellRequest, SystemRequest};
use qaqh_v3_transport::MuxTransport;
use tokio::io::AsyncRead;

use crate::v3_server::facade::BlobPush;
use crate::v3_server::harness;

const CONTROL_SAMPLES: usize = 300;
const PTY_SAMPLES: usize = 300;
const BLOB_BYTES: usize = 8 * 1024 * 1024;
// 1 KiB：与真实 PTY 读块的量级一致。刷得太猛会先把**自己的**有界队列打爆
// （服务端会正确地判 lag 并收流）——那测不到"隔离性"，只证明背压存在。
const PTY_FLOOD_CHUNK: usize = 1024;

const CONTROL_P99_LIMIT_MS: f64 = 50.0;
const PTY_P99_LIMIT_MS: f64 = 16.6;
const SESSION_B_P99_LIMIT_MS: f64 = 50.0;

pub(crate) async fn guard<B: MuxTransport>(pair: fn() -> (B, B)) {
    let state = harness::state();
    let (client_transport, server_transport) = pair();
    let (_server, _server_task) = harness::spawn_server(server_transport, Arc::clone(&state)).await;
    let mut client = harness::connect(client_transport, harness::owner_hello("guard-02")).await;
    let mut blobs = client.take_blob_inbox().expect("blob 收件箱");

    let session_a = harness::session_for(&state, &harness::owner_device());
    let session_b = harness::session_for(&state, &harness::owner_device());
    for session in [&session_a, &session_b] {
        client
            .call(ControlRequest::Session(SessionRequest::Attach {
                session_id: session.id.clone(),
                since_seq: 0,
            }))
            .await
            .expect("Attach 必须成功");
    }
    let stream_a = client
        .attach_session(&session_a.id, 0)
        .await
        .expect("会话 A 事件流");
    let stream_b = client
        .attach_session(&session_b.id, 0)
        .await
        .expect("会话 B 事件流");

    let flood_shell = harness::open_shell(&client).await;
    let echo_shell = harness::open_shell(&client).await;
    let pty_flood = client
        .attach_shell(&flood_shell, 0)
        .await
        .expect("刷屏 PTY 流");
    let mut pty_echo = client
        .attach_shell(&echo_shell, 0)
        .await
        .expect("量测 PTY 流");
    let flood_handle = state.shells.get(&flood_shell).expect("刷屏 shell 句柄");

    let blob_hash = state.blobs.put(vec![0xA5u8; BLOB_BYTES]);
    let blob_push = BlobPush {
        hash: blob_hash,
        len: BLOB_BYTES as u64,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let a_frames = Arc::new(AtomicU64::new(0));
    let pty_bytes = Arc::new(AtomicU64::new(0));
    let blob_bytes = Arc::new(AtomicU64::new(0));
    let b_arrivals: Arc<std::sync::Mutex<Vec<Duration>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // 洪峰 1：会话 A 的增量洪峰
    let flood_session = {
        let session = Arc::clone(&session_a);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let block_id = harness::block(101);
            let mut index = 0u64;
            while !stop.load(Ordering::Relaxed) {
                index += 1;
                session.emit(harness::answer(
                    &session.id,
                    &block_id,
                    format!("a{index:06};"),
                ));
                if index.is_multiple_of(16) {
                    tokio::task::yield_now().await;
                }
            }
            index
        })
    };
    // 洪峰 2：PTY 刷屏（服务端直推，不走控制面——测的正是"数据面 vs 控制面"的隔离）
    let flood_pty = {
        let handle = Arc::clone(&flood_handle);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let chunk = Bytes::from(vec![b'x'; PTY_FLOOD_CHUNK]);
            let mut index = 0u64;
            while !stop.load(Ordering::Relaxed) {
                index += 1;
                handle.push_output(chunk.clone());
                tokio::task::yield_now().await;
            }
            index
        })
    };
    // 洪峰 3：大 blob 分片推送
    let flood_blob = {
        let bus = state.push_bus.clone();
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut pushes = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if bus.send(blob_push).is_ok() {
                    pushes += 1;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            pushes
        })
    };
    // 会话 B 的心跳（每 5ms 一条）
    let heartbeat = {
        let session = Arc::clone(&session_b);
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut sent = 0u64;
            while !stop.load(Ordering::Relaxed) {
                session.emit(SessionEvent::CatalogChanged {
                    session_id: session.id.clone(),
                });
                sent += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            sent
        })
    };

    // 排干任务：不排干的话订阅队列会满 → lag → 流被收，那就不是在量延迟了。
    let drain_a = {
        let counter = Arc::clone(&a_frames);
        let mut stream = stream_a;
        tokio::spawn(async move {
            while stream.next_frame().await.is_ok() {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    let drain_pty = {
        let counter = Arc::clone(&pty_bytes);
        let mut stream = pty_flood;
        tokio::spawn(async move {
            while let Ok(frame) = stream.next_frame().await {
                counter.fetch_add(frame.bytes.len() as u64, Ordering::Relaxed);
            }
        })
    };
    let drain_blob = {
        let counter = Arc::clone(&blob_bytes);
        tokio::spawn(async move {
            while let Some(blob) = blobs.next().await {
                counter.fetch_add(blob.bytes.len() as u64, Ordering::Relaxed);
            }
        })
    };
    let drain_b = {
        let arrivals = Arc::clone(&b_arrivals);
        let mut stream = stream_b;
        tokio::spawn(async move {
            let mut last = Instant::now();
            loop {
                match tokio::time::timeout(Duration::from_secs(2), stream.next_frame()).await {
                    Ok(Ok(_)) => {
                        arrivals.lock().expect("arrivals").push(last.elapsed());
                        last = Instant::now();
                    }
                    // 2 秒没有一帧＝已被饿死：把这 2 秒记进样本，别静默退出。
                    Ok(Err(_)) => {
                        arrivals.lock().expect("arrivals").push(last.elapsed());
                        break;
                    }
                    Err(_) => {
                        arrivals.lock().expect("arrivals").push(last.elapsed());
                        break;
                    }
                }
            }
        })
    };

    // ── 阶段 1：控制面 RTT（Health，最便宜的真实 RPC） ──
    let mut control = Vec::with_capacity(CONTROL_SAMPLES);
    for _ in 0..CONTROL_SAMPLES {
        let started = Instant::now();
        client
            .call(ControlRequest::System(SystemRequest::Health))
            .await
            .expect("Health 必须在洪峰中仍可用");
        control.push(started.elapsed());
    }

    // ── 阶段 2：PTY 回显 RTT（Write + 等回显帧） ──
    let mut pty = Vec::with_capacity(PTY_SAMPLES);
    for index in 0..PTY_SAMPLES {
        let marker = format!("e{index:04}\n");
        let started = Instant::now();
        let write = client.call(ControlRequest::Shell(ShellRequest::Write {
            shell_id: echo_shell.clone(),
            bytes: Bytes::from(marker.clone()),
        }));
        let echo = read_until(&mut pty_echo, marker.as_bytes());
        let (written, ()) = tokio::join!(write, echo);
        match written.expect("PTY 写入必须成功") {
            ControlResponse::Shell(_) => {}
            other => panic!("PTY 写入回执形状不符：{other:?}"),
        }
        pty.push(started.elapsed());
    }

    stop.store(true, Ordering::Relaxed);
    let session_frames = flood_session.await.expect("会话洪峰任务");
    let pty_chunks = flood_pty.await.expect("PTY 洪峰任务");
    let blob_pushes = flood_blob.await.expect("blob 洪峰任务");
    let heartbeats = heartbeat.await.expect("心跳任务");
    let a_count = a_frames.load(Ordering::Relaxed);
    let pty_flooded = pty_bytes.load(Ordering::Relaxed);
    let blob_delivered = blob_bytes.load(Ordering::Relaxed);
    drain_a.abort();
    drain_pty.abort();
    drain_blob.abort();
    drain_b.abort();

    let arrivals = b_arrivals.lock().expect("arrivals").clone();
    let mut control_ref = control.clone();
    let mut pty_ref = pty.clone();
    let mut arrivals_ref = arrivals.clone();
    let control_p99 = harness::percentile_ms(&mut control_ref, 0.99);
    let control_p50 = harness::percentile_ms(&mut control.clone(), 0.5);
    let pty_p99 = harness::percentile_ms(&mut pty_ref, 0.99);
    let pty_p50 = harness::percentile_ms(&mut pty.clone(), 0.5);
    let b_p99 = harness::percentile_ms(&mut arrivals_ref, 0.99);
    let b_p50 = harness::percentile_ms(&mut arrivals.clone(), 0.5);

    println!("[v3-guard-02] 机器：{}", harness::machine_fingerprint());
    println!(
        "[v3-guard-02] 并发洪峰：会话A 发布 {session_frames} 帧（收到 {a_count}）；PTY 刷屏 {pty_chunks} 块 / {pty_flooded} B；blob 推送 {blob_pushes} 次 / 收到 {blob_delivered} B；会话B 心跳 {heartbeats} 条（样本 {}）",
        arrivals.len()
    );
    println!(
        "[v3-guard-02] 控制面 RTT n={} p50={control_p50:.3}ms p99={control_p99:.3}ms mean={:.3}ms max={:.3}ms（红线 P99 < {CONTROL_P99_LIMIT_MS}ms）",
        control.len(),
        harness::mean_ms(&control),
        harness::max_ms(&control)
    );
    println!(
        "[v3-guard-02] PTY 回显 RTT n={} p50={pty_p50:.3}ms p99={pty_p99:.3}ms mean={:.3}ms max={:.3}ms（红线 P99 ≤ {PTY_P99_LIMIT_MS}ms）",
        pty.len(),
        harness::mean_ms(&pty),
        harness::max_ms(&pty)
    );
    println!(
        "[v3-guard-02] 会话B 帧间隔 n={} p50={b_p50:.3}ms p99={b_p99:.3}ms（红线 P99 < {SESSION_B_P99_LIMIT_MS}ms）",
        arrivals.len()
    );

    // 洪峰必须真的压起来，否则延迟数字没有意义。
    assert!(a_count > 100, "会话 A 洪峰没跑起来（只收到 {a_count} 帧）");
    // 门槛只问"洪峰是否真的在跑"，不问"跑了多少"：量测阶段的长度随机器速度变化，
    // 把字节数绑死在某个绝对值上只会让守卫在不同宿主上假红。
    assert!(
        pty_flooded >= 256 * 1024,
        "PTY 刷屏没跑起来（只收到 {pty_flooded} B / {pty_chunks} 块）"
    );
    assert!(blob_delivered > 0, "大 blob 一次都没推成功");
    assert!(
        arrivals.len() >= 20,
        "会话 B 样本太少（{}），这条红线没被真正量到",
        arrivals.len()
    );

    assert!(
        control_p99 < CONTROL_P99_LIMIT_MS,
        "控制面 P99 {control_p99:.3}ms 超过红线 {CONTROL_P99_LIMIT_MS}ms：按 ADR-0042 第四节第 10 条应整体切 iroh，不得放宽阈值"
    );
    assert!(
        pty_p99 <= PTY_P99_LIMIT_MS,
        "PTY 回显 P99 {pty_p99:.3}ms 超过红线 {PTY_P99_LIMIT_MS}ms：按 ADR-0042 第四节第 10 条应整体切 iroh，不得放宽阈值"
    );
    assert!(
        b_p99 < SESSION_B_P99_LIMIT_MS,
        "会话 B 帧间隔 P99 {b_p99:.3}ms 超过红线 {SESSION_B_P99_LIMIT_MS}ms（被饿死）"
    );
}

/// 一直读到累计字节以 `needle` 结尾（回显是保序的，所以后缀匹配就够）。
async fn read_until<S>(stream: &mut ShellOutputStream<S>, needle: &[u8]) -> ()
where
    S: AsyncRead + Unpin,
{
    let mut seen: Vec<u8> = Vec::new();
    while !seen.ends_with(needle) {
        let frame = stream.next_frame().await.expect("PTY 回显帧");
        seen.extend_from_slice(&frame.bytes);
        assert!(
            seen.len() <= needle.len() * 4 + 4096,
            "回显流里出现了远多于预期的字节（串流？）"
        );
    }
}
